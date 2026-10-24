//! Merge explicit stack slots whose live ranges are disjoint (LLVM's
//! stack-slot coloring, fed by `llvm.lifetime.{start,end}` markers where
//! MIR emitted them). Today every `alloca` gets
//! its own `ExplicitSlot`, so a body that materializes one aggregate per
//! match arm — `parse_item_kind` holds ~40 × 136 B across 20 arms — pays
//! the sum in every frame (~18 KB vs LLVM's ~900 B for the same body).
//! Deep but finite recursion that fits the thread stack under LLVM then
//! overflows under us (stage2 rustc SIGBUS on
//! `tests/ui/expr/if/expr-stack-overflow.rs`), and big frames cost cache
//! and stack-probing even when they fit.
//!
//! For each slot we trace the `stack_addr` result through pure ops, block
//! params, and load/store/call args to find every instruction that can
//! touch the slot's bytes, giving a live interval over layout order.
//! Slots with disjoint intervals are folded onto one representative
//! (largest size / alignment wins) and absorbed slots shrink to zero.
//!
//! Pointers that escape — stored to memory, returned, or passed to a
//! possibly-writing callee — keep their slot live to the end of the
//! function. Two exceptions count as point uses instead: an `sret`
//! argument (the ABI forbids the callee retaining it) and any argument to
//! a write-free callee (`nowrite`), which has nowhere to stash a pointer.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::entity::{EntityRef, SecondaryMap};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    ArgumentPurpose, Block, BlockArg, FuncRef, Function, Inst, InstructionData, Opcode, StackSlot,
    Value, ValueDef, instructions::CallInfo,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

/// Is `v` an `iconst` of literal zero? `coldargs` disguises its cold-arg
/// rebinds as `umulhi x, 0` — a provable 0 whose result doesn't observe
/// the operand's address at all.
fn iconst_zero(func: &Function, v: Value) -> bool {
    let v = func.dfg.resolve_aliases(v);
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return false;
    };
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => {
            let bits = func.dfg.value_type(v).bits();
            let mask = if bits >= 64 {
                u64::MAX
            } else {
                (1u64 << bits) - 1
            };
            (imm.bits() as u64) & mask == 0
        }
        _ => false,
    }
}

// Worklist propagation of a slot-set map over a function: branch args
// flow into destination params (a try_call's *return* payload inherits
// the call args' union); every other inst's results take the union of
// its args' sets. `wl`/`inq` are the caller's initial worklist.
fn propagate(
    func: &Function,
    users: &SecondaryMap<Value, Vec<Inst>>,
    map: &mut SecondaryMap<Value, Vec<StackSlot>>,
    mut wl: VecDeque<Inst>,
    mut inq: FxHashSet<Inst>,
    contains: Option<&Vec<FxHashSet<StackSlot>>>,
    skip_loads: bool,
) {
    while let Some(i) = wl.pop_front() {
        inq.remove(&i);
        if func.dfg.insts[i].opcode().is_branch() {
            let call_slots: Vec<StackSlot> = if func.dfg.insts[i].opcode().is_call() {
                let mut v = Vec::new();
                for &a in func.dfg.inst_args(i) {
                    for &s in &map[func.dfg.resolve_aliases(a)] {
                        if !v.contains(&s) {
                            v.push(s);
                        }
                    }
                }
                // A callee can read through the pointer it was handed:
                // payloads it returns may name any slot reachable
                // through the arg's bytes, not just the arg's slot.
                if let Some(c) = contains {
                    let s0: Vec<StackSlot> = v.clone();
                    for s in s0 {
                        for &s2 in &c[s.index()] {
                            if !v.contains(&s2) {
                                v.push(s2);
                            }
                        }
                    }
                }
                v
            } else {
                Vec::new()
            };
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                let blk = bc.block(&func.dfg.value_lists);
                for (k, ba) in bc.args(&func.dfg.value_lists).enumerate() {
                    let src: Vec<StackSlot> = match ba {
                        BlockArg::Value(v) => map[func.dfg.resolve_aliases(v)].clone(),
                        // A normal-return payload can be the arg pointer
                        // itself (`fn(&T) -> &T`). The exception payload
                        // can't: the unwinder's object is fresh memory,
                        // and a pointer a callee could stash inside it
                        // already escaped via the call's own arg handling.
                        BlockArg::TryCallRet(_) => call_slots.clone(),
                        BlockArg::TryCallExn(_) => Vec::new(),
                    };
                    if src.is_empty() {
                        continue;
                    }
                    let Some(&p) = func.dfg.block_params(blk).get(k) else {
                        continue;
                    };
                    // A float param can't hold a pointer or a byte image
                    // of one; every other param type can carry them.
                    if func.dfg.value_type(p).is_float() {
                        continue;
                    }
                    let mut grew = false;
                    for &s in &src {
                        if !map[p].contains(&s) {
                            map[p].push(s);
                            grew = true;
                        }
                    }
                    if grew {
                        for &u in &users[p] {
                            if inq.insert(u) {
                                wl.push_back(u);
                            }
                        }
                    }
                }
            }
            continue;
        }
        // `umulhi x, 0` is a provable constant (coldargs' disguised-zero
        // rebind): the result carries no address, so the arg's slots must
        // not flow into it — otherwise the `iadd` rebind reads as
        // ptr + ptr and escapes the slot anyway.
        if func.dfg.insts[i].opcode() == Opcode::Umulhi
            && func.dfg.inst_args(i).iter().any(|&a| iconst_zero(func, a))
        {
            continue;
        }
        // A load result is the pointee's bytes, not an address derived
        // from the input: `derived` flow through loads is set precisely
        // by the `contains` fixpoint instead of inheriting the source
        // slot — otherwise plain data (`ushr`/`icmp` on a loaded word)
        // would read as observing the source slot's address.
        if skip_loads && func.dfg.insts[i].opcode().can_load() {
            continue;
        }
        let mut acc: Vec<StackSlot> = Vec::new();
        for &a in func.dfg.inst_args(i) {
            for &s in &map[func.dfg.resolve_aliases(a)] {
                if !acc.contains(&s) {
                    acc.push(s);
                }
            }
        }
        if acc.is_empty() {
            continue;
        }
        for &r in func.dfg.inst_results(i) {
            let r = func.dfg.resolve_aliases(r);
            let mut grew = false;
            for &s in &acc {
                if !map[r].contains(&s) {
                    map[r].push(s);
                    grew = true;
                }
            }
            if grew {
                for &u in &users[r] {
                    if inq.insert(u) {
                        wl.push_back(u);
                    }
                }
            }
        }
    }
}

/// Resolve a lifetime-marker operand to the slots it names, chasing
/// pointer-affine defs (`stack_addr`, `iadd` rebase, `bitcast`). Run this
/// where the operand is still live — mid-pipeline, the marker's `Value`
/// may already have been folded away, and `derived` at slotmerge time
/// would resolve to nothing.
pub fn marker_slots(func: &Function, v: Value) -> Vec<StackSlot> {
    let mut v = func.dfg.resolve_aliases(v);
    for _ in 0..8 {
        let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
            return Vec::new();
        };
        if let InstructionData::StackAddr { stack_slot, .. } = func.dfg.insts[i] {
            return vec![stack_slot];
        }
        match func.dfg.insts[i].opcode() {
            Opcode::Iadd | Opcode::Bitcast => {
                v = func.dfg.resolve_aliases(func.dfg.inst_args(i)[0]);
            }
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

fn sm_take() -> bool {
    static LIMIT: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    static USED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    match *LIMIT.get_or_init(|| crate::env_get("PLIRON_SM_LIMIT").and_then(|v| v.parse().ok())) {
        None => true,
        Some(n) => USED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < n,
    }
}

pub fn run(
    func: &mut Function,
    nowrite: &FxHashMap<FuncRef, bool>,
    lifetimes: &[(Vec<StackSlot>, Option<Inst>, Block, bool)],
    nocap: &FxHashSet<(Inst, u32)>,
) -> usize {
    if crate::env_set("PLIRON_SM_INFO") {
        eprintln!(
            "sm-info: slotmerge {} nslots={}",
            func.name,
            func.sized_stack_slots.len()
        );
    }
    if func.sized_stack_slots.len() < 2 {
        return 0;
    }

    // Intervals are measured in instruction positions over a CFG
    // reverse-postorder walk, not the final layout: cold blocks (shared
    // landing pads) sink to the tail in layout, which would stretch
    // every slot unwinding into them to function end — the same window
    // LLVM's stack coloring measures before machine sinking. The
    // exception edges of a `try_call` are CFG successors here, so a pad
    // measures next to the calls that can unwind to it. `bpos`/`bfirst`/
    // `blast` give each block's position range; `order` covers every
    // block (unreachable ones append at the end).
    let mut succ_map: SecondaryMap<Block, Vec<Block>> = SecondaryMap::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                succ_map[b].push(bc.block(&func.dfg.value_lists));
            }
        }
    }
    let mut order: Vec<Block> = Vec::with_capacity(func.layout.blocks().count());
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    if let Some(entry) = func.layout.entry_block() {
        seen.insert(entry);
        let mut stack = vec![(entry, 0usize)];
        while let Some(&(b, ci)) = stack.last() {
            if ci < succ_map[b].len() {
                stack.last_mut().unwrap().1 += 1;
                let s = succ_map[b][ci];
                if seen.insert(s) {
                    stack.push((s, 0));
                }
            } else {
                order.push(b);
                stack.pop();
            }
        }
    }
    order.reverse();
    for b in func.layout.blocks() {
        if seen.insert(b) {
            order.push(b);
        }
    }
    let mut bpos = SecondaryMap::<Block, u32>::new();
    let mut bfirst = SecondaryMap::<Block, u32>::new();
    let mut blast = SecondaryMap::<Block, u32>::new();
    let mut ipos = SecondaryMap::<Inst, u32>::new();
    let mut next = 0u32;
    for (i, &b) in order.iter().enumerate() {
        bpos[b] = i as u32;
        bfirst[b] = next;
        for i2 in func.layout.block_insts(b) {
            ipos[i2] = next;
            next += 1;
        }
        blast[b] = next.saturating_sub(1).max(bfirst[b]);
    }
    let last = next.saturating_sub(1);

    // insts reading each value — the propagation worklist's reverse edges.
    let mut users: SecondaryMap<Value, Vec<Inst>> = SecondaryMap::new();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                users[func.dfg.resolve_aliases(a)].push(i);
            }
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for ba in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = ba {
                        users[func.dfg.resolve_aliases(v)].push(i);
                    }
                }
            }
        }
    }

    // `derived[v]` = slots whose address `v` may hold, and
    // `from_bytes[v]` = slots whose byte image `v` may carry (a vector
    // load of an aggregate keeps the whole image — `extractlane`d lanes
    // can reconstitute stored pointers). Seeds: `stack_addr` results
    // (derived), and every load's results (from_bytes, keyed by the
    // address's slots). Both propagate over args like `derived` above;
    // a `try_call`'s payloads conservatively inherit call-arg unions.
    let mut derived: SecondaryMap<Value, Vec<StackSlot>> = SecondaryMap::new();
    let mut wl: VecDeque<Inst> = VecDeque::new();
    let mut inq: FxHashSet<Inst> = FxHashSet::default();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            if let InstructionData::StackAddr { stack_slot, .. } = func.dfg.insts[i]
                && let Some(&r) = func.dfg.inst_results(i).first()
            {
                let r = func.dfg.resolve_aliases(r);
                if !derived[r].contains(&stack_slot) {
                    derived[r].push(stack_slot);
                }
            }
            inq.insert(i);
            wl.push_back(i);
        }
    }
    propagate(func, &users, &mut derived, wl, inq, None, true);

    // `from_bytes` seeds: every load's results carry the byte image of
    // the slot its address derives from — an aggregate copy moves
    // contained pointers with it, so a store of the loaded bytes into
    // another slot must flow those pointers dst-ward.
    let mut from_bytes: SecondaryMap<Value, Vec<StackSlot>> = SecondaryMap::new();
    let mut wl: VecDeque<Inst> = VecDeque::new();
    let mut inq: FxHashSet<Inst> = FxHashSet::default();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            if func.dfg.insts[i].opcode().can_load() {
                if let Some(&a) = func.dfg.inst_args(i).first() {
                    for &s in &derived[func.dfg.resolve_aliases(a)] {
                        for &r in func.dfg.inst_results(i) {
                            let r = func.dfg.resolve_aliases(r);
                            if !from_bytes[r].contains(&s) {
                                from_bytes[r].push(s);
                            }
                        }
                    }
                }
            }
            inq.insert(i);
            wl.push_back(i);
        }
    }
    propagate(func, &users, &mut from_bytes, wl, inq, None, false);

    // Per-slot use positions and live interval [lo, hi] (`u32::MAX` hi is
    // normalized to `last` afterwards).
    let nslots = func.sized_stack_slots.len();
    let mut iv: Vec<(u32, u32)> = vec![(u32::MAX, 0); nslots];
    let mut uses: Vec<Vec<u32>> = vec![Vec::new(); nslots];
    // Per-slot byte accesses. `reads` can observe stored bytes; `writes`
    // definitely store them. A backward-edge span must stretch the
    // interval of a slot whose bytes can be live across trips — reads in
    // the span not dominated by an in-span write — but a slot every
    // in-span read of which is dominated by an in-span write is
    // re-initialized each trip, so its plain use window already suffices.
    let mut reads: Vec<Vec<Inst>> = vec![Vec::new(); nslots];
    let mut writes: Vec<Vec<Inst>> = vec![Vec::new(); nslots];
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::slotmerge", tracing::Level::DEBUG);
    let mut esc: Vec<(StackSlot, Inst)> = Vec::new();
    // `llvm.lifetime.{start,end}` markers (MIR StorageLive/Dead) pin
    // validity windows in LLVM's model: a pointer into dead storage can't
    // be validly dereferenced, so escapes couldn't extend a marked slot's
    // live range past StorageDead. In practice rustc's MIR doesn't uphold
    // that strongly enough — stage2 `lower_to_hir` read a corrupted
    // DefIndex when escapes were exempted — so markers only contribute
    // use positions. `PLIRON_SLOTMERGE_MARK` re-enables the exemption for
    // experiments.
    //
    // Positions resolve eagerly because anchors recorded at lowering decay
    // mid-pipeline: a pass can delete the anchor inst or fold its whole
    // block. A slot is only `marked` when *every* singleton marker on it
    // resolves — one lost end marker leaves the window unbounded, which
    // is worse than no marker at all (the escape exemption would clip a
    // live range the source semantics extended to function end).
    let in_layout: FxHashSet<Block> = order.iter().copied().collect();
    let mut marked = vec![false; nslots];
    let mut mark_lost = vec![false; nslots];
    let mut marker_pos: Vec<Option<u32>> = Vec::new();
    if crate::env_set("PLIRON_SLOTMERGE_MARK") {
        let (mut single, mut multi, mut none, mut lost) = (0u32, 0u32, 0u32, 0u32);
        marker_pos.reserve(lifetimes.len());
        for (ds, anchor, b, start) in lifetimes {
            // The marker sits right after the anchor instruction: a start
            // marker is live from the next position, an end marker's last
            // live point is the anchor itself. No usable anchor degrades
            // to the block's start, and a block that left the layout
            // resolves to nothing.
            let pos = match anchor {
                &Some(i) if func.layout.inst_block(i).is_some() => {
                    if *start {
                        (ipos[i] + 1).min(last)
                    } else {
                        ipos[i]
                    }
                }
                _ if in_layout.contains(b) => bfirst[*b],
                _ => u32::MAX,
            };
            marker_pos.push((pos != u32::MAX).then_some(pos));
            // A marker naming exactly one slot is that storage's window;
            // a unioned (phi-derived) operand can't be trusted as a
            // per-slot bound, so ambiguous markers mark nothing.
            match ds.as_slice() {
                &[s] if s.index() < nslots => {
                    if pos == u32::MAX {
                        mark_lost[s.index()] = true;
                        lost += 1;
                    } else {
                        marked[s.index()] = true;
                        single += 1;
                    }
                }
                [] => none += 1,
                _ => multi += 1,
            }
        }
        for s in 0..nslots {
            marked[s] &= !mark_lost[s];
        }
        if crate::env_set("PLIRON_SM_INFO") {
            eprintln!(
                "sm-info: markers: {single} singleton {multi} union {none} empty {lost} lost"
            );
        }
    }
    // An escaped slot can never fold: its address reached an opaque
    // sink, so sharing storage with anything could flip a retained
    // pointer's identity — interval disjointness can't rule that out.
    // `esc_seen` records escapes on marked slots too: markers widen a
    // marked-escaped slot's interval to its declared window below.
    let mut escaped = vec![false; nslots];
    let mut esc_seen = vec![false; nslots];
    macro_rules! escape {
        ($s:expr, $i:expr) => {{
            esc_seen[$s.index()] = true;
            if !marked[$s.index()] {
                iv[$s.index()].1 = u32::MAX;
                escaped[$s.index()] = true;
            }
            if dbg || crate::env_set("PLIRON_SM_INFO") {
                esc.push(($s, $i));
            }
        }};
    }
    // Escaping a pointer to `s` exposes `s`'s bytes — including pointers
    // stored inside it — to whoever holds the escapee. Escape `s` and
    // the whole contains-reach. (`contains` no longer self-seeds, so the
    // subject is escaped explicitly here.)
    macro_rules! escape_deep {
        ($c:expr, $s:expr, $i:expr) => {{
            escape!($s, $i);
            for s2 in $c[$s.index()].clone() {
                escape!(s2, $i);
            }
        }};
    }
    // (stored, container): `&stored` was written into `container`'s bytes.
    // A reload through the container can expose the pointer at any point
    // in the container's live range, so the stored slot's interval must
    // contain the container's — still bounded when the container is.
    let mut edges: FxHashSet<(StackSlot, StackSlot)> = FxHashSet::default();
    // (src, dst): src's byte image was copied into dst — a `memcpy`/
    // `memmove` libcall or an aggregate load/store move — so pointers
    // contained in src are now contained in dst as well.
    let mut cpy: FxHashSet<(StackSlot, StackSlot)> = FxHashSet::default();
    // contains[d] = slots a pointer reloaded out of d's bytes may name:
    // every slot whose pointer was stored into d (d itself only via an
    // actual self-store), and the closure through byte-copies.
    let mut contains: Vec<FxHashSet<StackSlot>> =
        (0..nslots).map(|_| FxHashSet::default()).collect();

    // The prepass edges are built from `derived`, and `derived` gains
    // entries once load results resolve through `contains` — a reloaded
    // pointer's pointee is only known after the pointer was stored —
    // so iterate the three to a joint fixpoint. All inserts are
    // monotone; a pathological function that fails to converge still
    // exits soundly (unmerged is always safe).
    for round in 0..8 {
        let mut grew = 0usize;
        for &b in &order {
            for i in func.layout.block_insts(b) {
                let d = &func.dfg.insts[i];
                let op = d.opcode();
                let args = func.dfg.inst_args(i).to_vec();
                let aslot = |k: usize| -> &[StackSlot] {
                    args.get(k)
                        .map(|&a| derived[func.dfg.resolve_aliases(a)].as_slice())
                        .unwrap_or(&[])
                };
                if op.is_call() {
                    let info = d.analyze_call(&func.dfg.value_lists, &func.dfg.exception_tables);
                    if let CallInfo::Direct(fr, cargs) | CallInfo::DirectWithSig(fr, _, cargs) =
                        info
                    {
                        use cranelift_codegen::ir::{ExternalName, LibCall};
                        if cargs.len() >= 2
                            && matches!(
                                func.dfg.ext_funcs[fr].name,
                                ExternalName::LibCall(LibCall::Memcpy)
                                    | ExternalName::LibCall(LibCall::Memmove)
                            )
                        {
                            for &x in &derived[func.dfg.resolve_aliases(cargs[1])] {
                                for &d2 in &derived[func.dfg.resolve_aliases(cargs[0])] {
                                    if x != d2 {
                                        grew += cpy.insert((x, d2)) as usize;
                                    }
                                }
                            }
                        }
                    }
                } else if op.can_store() {
                    match op {
                        Opcode::Store
                        | Opcode::Istore8
                        | Opcode::Istore16
                        | Opcode::Istore32
                        | Opcode::AtomicStore => {
                            let dst: Vec<StackSlot> = aslot(1).to_vec();
                            for &s in aslot(0) {
                                for &d2 in &dst {
                                    // A self-store (s == d2) is real: the
                                    // slot's bytes then name the slot.
                                    grew += edges.insert((s, d2)) as usize;
                                }
                            }
                            for &s in &from_bytes[func.dfg.resolve_aliases(args[0])] {
                                for &d2 in &dst {
                                    if s != d2 {
                                        grew += cpy.insert((s, d2)) as usize;
                                    }
                                }
                            }
                        }
                        Opcode::AtomicRmw | Opcode::AtomicCas => {
                            let dst: Vec<StackSlot> = aslot(0).to_vec();
                            for k in 1..args.len() {
                                for &s in aslot(k) {
                                    for &d2 in &dst {
                                        grew += edges.insert((s, d2)) as usize;
                                    }
                                }
                                for &s in &from_bytes[func.dfg.resolve_aliases(args[k])] {
                                    for &d2 in &dst {
                                        if s != d2 {
                                            grew += cpy.insert((s, d2)) as usize;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // `contains` fixpoint: direct stored pointers plus the
        // transitive closure over byte-copies.
        loop {
            let mut changed = false;
            for &(s, d) in &edges {
                if contains[d.index()].insert(s) {
                    changed = true;
                }
            }
            for &(x, d) in &cpy {
                let add: Vec<StackSlot> = contains[x.index()].iter().copied().collect();
                for s in add {
                    if contains[d.index()].insert(s) {
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // A load result may hold the address of any slot the source's
        // bytes contain — the reconstituted pointer attributes its
        // uses to the pointee, not the container.
        let mut wl2: VecDeque<Inst> = VecDeque::new();
        let mut inq2: FxHashSet<Inst> = FxHashSet::default();
        for &b in &order {
            for i in func.layout.block_insts(b) {
                if !func.dfg.insts[i].opcode().can_load() {
                    continue;
                }
                let Some(&a) = func.dfg.inst_args(i).first() else {
                    continue;
                };
                // The loaded bytes name the slots stored into the
                // pointee — not the pointee itself, unless it was
                // actually self-stored (`contains` doesn't self-seed).
                let mut acc: Vec<StackSlot> = Vec::new();
                for &s0 in &derived[func.dfg.resolve_aliases(a)] {
                    for &s in &contains[s0.index()] {
                        if !acc.contains(&s) {
                            acc.push(s);
                        }
                    }
                }
                for &r in func.dfg.inst_results(i) {
                    let r = func.dfg.resolve_aliases(r);
                    let mut grew2 = false;
                    for &s in &acc {
                        if !derived[r].contains(&s) {
                            derived[r].push(s);
                            grew2 = true;
                            grew += 1;
                        }
                    }
                    if grew2 {
                        for &u in &users[r] {
                            if inq2.insert(u) {
                                wl2.push_back(u);
                            }
                        }
                    }
                }
            }
        }
        propagate(func, &users, &mut derived, wl2, inq2, Some(&contains), true);
        if grew == 0 {
            break;
        }
        if round == 7 {
            return 0;
        }
    }
    let use_at = |iv: &mut Vec<(u32, u32)>, uses: &mut Vec<Vec<u32>>, s: StackSlot, p: u32| {
        uses[s.index()].push(p);
        let e = &mut iv[s.index()];
        e.0 = e.0.min(p);
        e.1 = e.1.max(p);
    };
    {
        for &b in &order {
            for i in func.layout.block_insts(b) {
                let pos = ipos[i];
                let d = &func.dfg.insts[i];
                let op = d.opcode();
                // `stack_addr` itself is not a use: taking the address
                // doesn't touch the slot's bytes, and treating it as one
                // would extend every interval to the (hoisted) addr site.
                if let &InstructionData::StackAddr { .. } = d {
                    continue;
                }
                if op.is_branch() && !op.is_call() {
                    continue; // control flow only; args handled by params
                }
                let args = func.dfg.inst_args(i).to_vec();
                let arg_slots = |k: usize| -> &[StackSlot] {
                    args.get(k)
                        .map(|&a| derived[func.dfg.resolve_aliases(a)].as_slice())
                        .unwrap_or(&[])
                };
                if op.is_call() {
                    match op {
                        // A tail-called callee can outlive this frame; a
                        // stack switch may touch it from another stack.
                        Opcode::ReturnCall | Opcode::ReturnCallIndirect | Opcode::StackSwitch => {
                            for k in 0..args.len() {
                                for &s in arg_slots(k) {
                                    use_at(&mut iv, &mut uses, s, pos);
                                    reads[s.index()].push(i);
                                    escape_deep!(contains, s, i);
                                }
                            }
                        }
                        _ => {
                            let info =
                                d.analyze_call(&func.dfg.value_lists, &func.dfg.exception_tables);
                            let plain = matches!(info, CallInfo::Direct(..));
                            match info {
                                CallInfo::Direct(fr, cargs)
                                | CallInfo::DirectWithSig(fr, _, cargs) => {
                                    let sret = func.dfg.signatures
                                        [func.dfg.ext_funcs[fr].signature]
                                        .params
                                        .first()
                                        .is_some_and(|p| {
                                            p.purpose == ArgumentPurpose::StructReturn
                                        });
                                    // nowrite `true` never writes; `false`
                                    // may write on panic paths. A plain call
                                    // unwinds us either way (our frame dies
                                    // with it), so both count; a try_call
                                    // lands in our own pads, which could
                                    // observe a pointer the callee stashed
                                    // while panicking — strict only.
                                    let nw = if plain {
                                        nowrite.contains_key(&fr)
                                    } else {
                                        nowrite.get(&fr) == Some(&true)
                                    };
                                    // libc mem* copies through the pointers
                                    // but never retains them (the copy
                                    // edges are collected in the prepass).
                                    use cranelift_codegen::ir::{ExternalName, LibCall};
                                    let copier = matches!(
                                        func.dfg.ext_funcs[fr].name,
                                        ExternalName::LibCall(LibCall::Memcpy)
                                            | ExternalName::LibCall(LibCall::Memmove)
                                    );
                                    let nokeep = copier
                                        || matches!(
                                            func.dfg.ext_funcs[fr].name,
                                            ExternalName::LibCall(LibCall::Memset)
                                        );
                                    for (k, &a) in cargs.iter().enumerate() {
                                        let ds = derived[func.dfg.resolve_aliases(a)].clone();
                                        for &s in &ds {
                                            use_at(&mut iv, &mut uses, s, pos);
                                        }
                                        // A mem* dst arg definitely writes
                                        // its (single) pointee; other args
                                        // may observe stale bytes.
                                        if nokeep && k == 0 {
                                            if let &[s] = ds.as_slice() {
                                                writes[s.index()].push(i);
                                            }
                                        } else {
                                            for &s in &ds {
                                                reads[s.index()].push(i);
                                            }
                                        }
                                        // A memcpy/memmove into untracked
                                        // memory carries the source's byte
                                        // image — and every pointer inside
                                        // it — somewhere we can't follow.
                                        // The copy edges only form for
                                        // slot dsts, so the contents escape
                                        // here (the container's own address
                                        // stays put, so it does not).
                                        if copier && k == 1 && arg_slots(0).is_empty() {
                                            for &s in &ds {
                                                for s2 in contains[s.index()].clone() {
                                                    escape!(s2, i);
                                                }
                                            }
                                        }
                                        for &s in &ds {
                                            // rustc's captures(none): the
                                            // callee can't retain the
                                            // pointer past the call.
                                            if !nw
                                                && !nokeep
                                                && !(sret && k == 0)
                                                && !nocap.contains(&(i, k as u32))
                                            {
                                                escape_deep!(contains, s, i);
                                            } else if !nokeep && !(sret && k == 0) {
                                                // ...but only that pointer:
                                                // one the callee loads *out
                                                // of* the pointee is fair
                                                // game — it can hand it back
                                                // (cranelift's
                                                // `compute_use_states`
                                                // closure returns an iterator
                                                // built from `*env`), so the
                                                // contained slots escape.
                                                for s2 in contains[s.index()].clone() {
                                                    escape!(s2, i);
                                                }
                                            }
                                        }
                                    }
                                }
                                CallInfo::Indirect(_, cargs) => {
                                    for (k, &a) in cargs.iter().enumerate() {
                                        for &s in &derived[func.dfg.resolve_aliases(a)] {
                                            use_at(&mut iv, &mut uses, s, pos);
                                            reads[s.index()].push(i);
                                            if !nocap.contains(&(i, k as u32)) {
                                                escape_deep!(contains, s, i);
                                            } else {
                                                // As for direct calls: the
                                                // pointee's pointers escape.
                                                for s2 in contains[s.index()].clone() {
                                                    escape!(s2, i);
                                                }
                                            }
                                        }
                                    }
                                }
                                CallInfo::NotACall => {}
                            }
                        }
                    }
                } else if op.is_return() {
                    // A returned pointer outlives the frame either way:
                    // after `return` nothing can access the slot again on
                    // this path, so it adds no live-range constraint.
                } else if op.can_store() {
                    match op {
                        // store v, p / atomic_store v, p — a stored
                        // pointer escapes; the address is the use. When
                        // `p` is itself slot-derived the pointer stays
                        // inside our frame: a container edge bounds the
                        // escape to the container's live range instead.
                        Opcode::Store
                        | Opcode::Istore8
                        | Opcode::Istore16
                        | Opcode::Istore32
                        | Opcode::AtomicStore => {
                            let dst: Vec<StackSlot> = arg_slots(1).to_vec();
                            for &s in arg_slots(0) {
                                use_at(&mut iv, &mut uses, s, pos);
                                if dst.is_empty() {
                                    escape_deep!(contains, s, i);
                                }
                            }
                            if dst.is_empty() {
                                // A stored byte image leaks every pointer
                                // it carried — the container itself is
                                // only a use when the image lands in a
                                // tracked slot (copy edge covers it).
                                for &s in &from_bytes[func.dfg.resolve_aliases(args[0])] {
                                    escape_deep!(contains, s, i);
                                }
                            }
                            // `store` through a pointer that can only
                            // name `s` is a definite write of `s`'s
                            // bytes; a maybe-pointer can't reinit.
                            if let &[s] = arg_slots(1) {
                                writes[s.index()].push(i);
                            }
                            for &s in arg_slots(1) {
                                use_at(&mut iv, &mut uses, s, pos);
                            }
                        }
                        // atomic_rmw p, v / atomic_cas p, e, n
                        Opcode::AtomicRmw | Opcode::AtomicCas => {
                            let dst: Vec<StackSlot> = arg_slots(0).to_vec();
                            for &s in &dst {
                                use_at(&mut iv, &mut uses, s, pos);
                                reads[s.index()].push(i);
                            }
                            for k in 1..args.len() {
                                for &s in arg_slots(k) {
                                    use_at(&mut iv, &mut uses, s, pos);
                                    if dst.is_empty() {
                                        escape_deep!(contains, s, i);
                                    }
                                }
                                if dst.is_empty() {
                                    for &s in &from_bytes[func.dfg.resolve_aliases(args[k])] {
                                        escape_deep!(contains, s, i);
                                    }
                                }
                            }
                        }
                        _ => {
                            for k in 0..args.len() {
                                for &s in arg_slots(k) {
                                    use_at(&mut iv, &mut uses, s, pos);
                                    reads[s.index()].push(i);
                                    escape!(s, i);
                                }
                            }
                        }
                    }
                } else if op.can_load() {
                    for &s in arg_slots(0) {
                        use_at(&mut iv, &mut uses, s, pos);
                        reads[s.index()].push(i);
                    }
                    for k in 1..args.len() {
                        for &s in arg_slots(k) {
                            use_at(&mut iv, &mut uses, s, pos);
                            escape_deep!(contains, s, i);
                        }
                    }
                } else if op.other_side_effects() || op.is_terminator() {
                    for k in 0..args.len() {
                        for &s in arg_slots(k) {
                            use_at(&mut iv, &mut uses, s, pos);
                            reads[s.index()].push(i);
                            escape_deep!(contains, s, i);
                        }
                    }
                } else {
                    // Pure ops on a slot-derived value observe the
                    // address itself, not the slot's bytes — folding the
                    // slot onto another would change the result
                    // (`icmp`/`isub` ptrdiff/bit ops/truncation). Only
                    // pointer-affine ops keep a faithful same-slot
                    // address; everything else escapes the slot.
                    let affine = match op {
                        Opcode::Select
                        | Opcode::Uextend
                        | Opcode::Bitcast
                        | Opcode::Splat
                        | Opcode::Insertlane
                        | Opcode::Extractlane => true,
                        // ptr + scalar / ptr - scalar stays a pointer
                        // into the same slot; ptr + ptr / ptr - ptr is
                        // data — an identity observation.
                        Opcode::Iadd => arg_slots(0).is_empty() || arg_slots(1).is_empty(),
                        Opcode::Isub => arg_slots(1).is_empty(),
                        // `umulhi x, 0` is a constant — the result
                        // doesn't observe the operand's address
                        // (coldargs' disguised-zero rebind).
                        Opcode::Umulhi => args.iter().any(|&a| iconst_zero(func, a)),
                        _ => false,
                    };
                    if !affine {
                        for k in 0..args.len() {
                            for &s in arg_slots(k) {
                                use_at(&mut iv, &mut uses, s, pos);
                                escape_deep!(contains, s, i);
                            }
                        }
                    }
                }
            }
        }
    }
    // Marker positions widen a marked-escaped slot's interval to its
    // declared window — opaque accesses through a retained pointer are
    // covered while the marker says the storage lives. Everywhere else
    // real uses already bound the interval; extra marker positions would
    // just stretch it (a marker-only slot is dead storage the unused-slot
    // sweep reclaims anyway).
    for ((slots, _, _, _), pos) in lifetimes.iter().zip(&marker_pos) {
        let Some(pos) = pos else { continue };
        for &s in slots {
            if s.index() < nslots && marked[s.index()] && esc_seen[s.index()] {
                use_at(&mut iv, &mut uses, s, *pos);
            }
        }
    }

    for e in iv.iter_mut() {
        if e.1 == u32::MAX && e.0 != u32::MAX {
            e.1 = last;
        }
    }

    // A backward edge lets a path re-enter an earlier region. A slot
    // whose in-span reads can observe bytes written on an earlier trip —
    // any read not dominated by an in-span write of the same slot —
    // stays live across the whole span. Slot addresses are fixed, so a
    // carried pointer alone doesn't force this; only stale data does.
    let mut spans: Vec<(u32, u32)> = Vec::new();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                let v = bc.block(&func.dfg.value_lists);
                // `<=`: a self-loop is a backedge too.
                if bpos[v] <= bpos[b] {
                    spans.push((bfirst[v], blast[b]));
                    if crate::env_set("PLIRON_SM_INFO") {
                        eprintln!(
                            "sm-info: backedge {b}->{v} span=({},{})",
                            bfirst[v], blast[b]
                        );
                    }
                }
            }
        }
    }
    // A span is (header start, back-edge source end) in RPO positions,
    // but RPO doesn't keep a loop's blocks contiguous: a nested loop
    // whose header lies inside span A can have its body (and back edges)
    // placed past A's end, and paths through that body still return to
    // A's header. cranelift's `compute_use_states` lost a 72-byte
    // iterator slot that way (`PLIRON_SM_POISON` found it). Close the
    // spans: A grows to cover any span whose header sits inside it.
    loop {
        let mut grew = false;
        for a in 0..spans.len() {
            for b in 0..spans.len() {
                let (alo, ahi) = spans[a];
                let (blo, bhi) = spans[b];
                if alo <= blo && blo <= ahi && bhi > ahi {
                    spans[a].1 = bhi;
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    if crate::env_set("PLIRON_SM_INFO") {
        let mut big: Vec<(u32, u32)> = spans.clone();
        big.sort();
        big.dedup();
        big.sort_by_key(|&(lo, hi)| hi - lo);
        eprintln!(
            "sm-info: {} backedge spans, widest: {:?}",
            spans.len(),
            big.iter().rev().take(6).collect::<Vec<_>>()
        );
    }
    if !spans.is_empty() {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        // A read of a container is a read of every slot whose address it
        // holds: the callee dereferences through it. prqlc's closure env
        // held `&requirements`; the calls read only the env, so the
        // once-written `requirements` slot looked fresh in the loop and
        // `log` temps were merged onto it (2026-10-22).
        let mut via: Vec<Vec<Inst>> = vec![Vec::new(); nslots];
        for d in 0..nslots {
            for &s in &contains[d] {
                if s.index() < nslots {
                    via[s.index()].extend(reads[d].iter().copied());
                }
            }
        }
        for (s, e) in iv.iter_mut().enumerate() {
            if marked[s] {
                continue;
            }
            for &(lo, hi) in &spans {
                if !uses[s].iter().any(|&p| lo <= p && p <= hi) {
                    continue;
                }
                // A read after the span sees bytes any trip may have
                // written: partial in-loop writes accumulate across trips
                // (smallsort's bidirectional merge fills its scratch one
                // element per trip at each end; the copy-back after the
                // loop reads it all), so they are live across the back
                // edge — unless a whole-slot write in or after the span
                // dominates the read and supersedes them.
                let fresh = reads[s].iter().chain(&via[s]).all(|&r| {
                    let rp = ipos[r];
                    if rp < lo {
                        return true;
                    }
                    let after = rp > hi;
                    writes[s].iter().any(|&w| {
                        let wp = ipos[w];
                        lo <= wp
                            && (after || wp <= hi)
                            && covers_slot(func, w, StackSlot::from_u32(s as u32))
                            && dt.dominates(w, r, &func.layout)
                    })
                });
                if !fresh {
                    e.0 = e.0.min(lo);
                    e.1 = e.1.max(hi);
                }
            }
        }
    }

    // `contains` is closed over stores and byte-copies: a slot reachable
    // through another's bytes stays reachable for that container's whole
    // live range, so its interval must contain the container's. (The
    // store/copy edges were gathered before the scan; `contains` is the
    // fixpoint's result — iterate just the interval extension since
    // growing `d`'s range can widen `s`'s in turn.)
    loop {
        let mut changed = false;
        for d in 0..nslots {
            let (dlo, dhi) = iv[d];
            for &s in &contains[d] {
                let e = &mut iv[s.index()];
                if e.0 != u32::MAX {
                    if e.0 > dlo {
                        e.0 = dlo;
                        changed = true;
                    }
                    if e.1 < dhi {
                        e.1 = dhi;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    if dbg {
        let mut cc: Vec<(usize, usize)> = contains
            .iter()
            .enumerate()
            .map(|(s, c)| (s, c.len()))
            .filter(|&(_, n)| n > 4)
            .collect();
        cc.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::slotmerge", "slotmerge: biggest contains: {:?}",
                cc.iter().take(8).map(|&(s, n)| (StackSlot::from_u32(s as u32), n)).collect::<Vec<_>>())
        };
        for s in 0..nslots {
            let s = StackSlot::from_u32(s as u32);
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::slotmerge", "slotmerge: {s} iv={:?} uses={:?}",
                    iv[s.index()],
                    uses[s.index()],
                )
            };
        }
        for &(s, i) in &esc {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::slotmerge", "slotmerge: {s} escapes at {i} {}: {}",
                    func.layout.inst_block(i).unwrap(),
                    func.dfg.display_inst(i),
                )
            };
        }
    }
    if crate::env_set("PLIRON_SM_INFO") {
        let mut unbounded = 0;
        let mut bounded = 0;
        let mut whole = 0;
        for e in &iv {
            if e.0 == u32::MAX {
                unbounded += 1;
            } else {
                bounded += 1;
                if e.0 <= 1 && e.1 + 64 >= last {
                    whole += 1;
                }
            }
        }
        eprintln!(
            "sm-info: {nslots} slots: {bounded} bounded ({whole} whole-fn), \
             {unbounded} unused, {} escaped, {} marked",
            escaped.iter().filter(|&&x| x).count(),
            marked.iter().filter(|&&x| x).count(),
        );
        let mut hist: FxHashMap<(u32, u32), u32> = FxHashMap::default();
        for (s, e) in iv.iter().enumerate() {
            if e.0 != u32::MAX && escaped[s] {
                *hist.entry((e.0 / 5000, e.1 / 5000)).or_default() += 1;
            }
        }
        let mut h: Vec<_> = hist.iter().collect();
        h.sort_by_key(|&(_, n)| std::cmp::Reverse(*n));
        eprintln!("sm-info: escaped-slot interval buckets (lo,hi)/5k: {h:?}");
        let mut hist2: FxHashMap<u32, u32> = FxHashMap::default();
        let mut spanb = vec![0u64; 8];
        for (s, e) in iv.iter().enumerate() {
            if e.0 != u32::MAX && !escaped[s] {
                *hist2.entry(e.1 - e.0).or_default() += 1;
                let span = e.1 - e.0;
                let b = match span {
                    0..=63 => 0,
                    64..=255 => 1,
                    256..=1023 => 2,
                    1024..=4095 => 3,
                    4096..=16383 => 4,
                    _ => 5,
                };
                spanb[b] += 1;
            }
        }
        let mut h2: Vec<_> = hist2.iter().map(|(&k, &n)| (k, n)).collect();
        h2.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        eprintln!(
            "sm-info: bounded interval lens (top10): {:?} span-hist: {spanb:?} last={last}",
            h2.iter().take(10).collect::<Vec<_>>()
        );
        let mut ops: FxHashMap<String, u32> = FxHashMap::default();
        for &(_, i) in &esc {
            *ops.entry(func.dfg.insts[i].opcode().to_string())
                .or_default() += 1;
        }
        let mut opv: Vec<_> = ops.into_iter().collect();
        opv.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        eprintln!("sm-info: escape insts by opcode: {opv:?}");
        let mut by_fn: FxHashMap<String, u32> = FxHashMap::default();
        for &(_, i) in &esc {
            if let InstructionData::TryCall { func_ref, .. }
            | InstructionData::Call { func_ref, .. } = func.dfg.insts[i]
            {
                *by_fn
                    .entry(format!(
                        "{func_ref}/{:?}",
                        func.dfg.ext_funcs[func_ref].name
                    ))
                    .or_default() += 1;
            }
        }
        let mut fnv: Vec<_> = by_fn.into_iter().collect();
        fnv.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        eprintln!("sm-info: escape insts by callee: {fnv:?}");
        for &(s, i) in esc.iter().take(8) {
            eprintln!(
                "sm-info:   {s} escapes at {}: {}",
                func.layout.inst_block(i).unwrap(),
                func.dfg.display_inst(i)
            );
        }
    }
    // `PLIRON_SM_POISON=1`: a use-after-scope check for this analysis.
    // Merge nothing; instead overwrite each slot that coloring would have
    // considered with 0xA5 bytes wherever the analysis says it is dead —
    // so a wrong interval reads garbage at its first stale access, in
    // every run, whether or not a merge would have reused the bytes.
    if crate::env_set("PLIRON_SM_POISON") {
        return poison_dead(func, &order, &bfirst, &blast, &succ_map, &iv, &escaped);
    }

    // Greedy interval coloring, best-fit by region size.
    let mut cand: Vec<(u32, StackSlot)> = (0..nslots)
        .map(|s| StackSlot::from_u32(s as u32))
        .filter(|&s| iv[s.index()].0 != u32::MAX && !escaped[s.index()])
        .map(|s| (iv[s.index()].0, s))
        .collect();
    cand.sort();
    // (region end, region size, region align_shift, representative)
    let mut regions: Vec<(u32, u32, u8, StackSlot)> = Vec::new();
    let mut rep: FxHashMap<StackSlot, StackSlot> = FxHashMap::default();
    for (_, s) in cand {
        let (lo, hi) = iv[s.index()];
        let (size, align) = {
            let d = &func.sized_stack_slots[s];
            (d.size, d.align_shift)
        };
        let mut best: Option<usize> = None;
        for (r, &(end, rsize, ralign, _)) in regions.iter().enumerate() {
            if end < lo
                && rsize >= size
                && ralign >= align
                && best.is_none_or(|b| regions[b].1 > rsize)
            {
                best = Some(r);
            }
        }
        // `PLIRON_SM_LIMIT=n`: accept only the first n merges in this
        // process (delta-debug a bad merge; pair with PLIRON_OPT_FNS).
        let best = best.filter(|_| sm_take());
        match best {
            Some(r) => {
                rep.insert(s, regions[r].3);
                regions[r].0 = hi;
            }
            None => {
                rep.insert(s, s);
                regions.push((hi, size, align, s));
            }
        }
    }
    if crate::env_set("PLIRON_SM_INFO") {
        let ncand = rep.len();
        eprintln!("sm-info: merge: {ncand} cand -> {} regions", regions.len());
        for s in 0..nslots {
            let ss = StackSlot::from_u32(s as u32);
            if iv[s].0 != u32::MAX {
                eprintln!(
                    "sm-info:   {ss} iv={:?} esc={} seen={} marked={} rep={:?}",
                    iv[s],
                    escaped[s],
                    esc_seen[s],
                    marked[s],
                    rep.get(&ss)
                );
            }
        }
    }

    // Rewrite `stack_addr`s onto representatives; grow each rep to its
    // largest member and zero the absorbed slots. Slots with no live
    // interval may still be referenced by `stack_addr` insts whose
    // derived uses we failed to track — keep their size unless no
    // `stack_addr` in the function names them at all.
    let mut addr_refs: FxHashSet<StackSlot> = FxHashSet::default();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            if let InstructionData::StackAddr { stack_slot, .. } = func.dfg.insts[i] {
                addr_refs.insert(stack_slot);
            }
        }
    }
    let mut merged = 0;
    let mut grown: Vec<(StackSlot, u32, u8)> = Vec::new();
    for s in 0..nslots {
        let s = StackSlot::from_u32(s as u32);
        match rep.get(&s).copied() {
            None => {
                if iv[s.index()].0 == u32::MAX && !addr_refs.contains(&s) {
                    func.sized_stack_slots[s].size = 0;
                }
            }
            Some(r) if r != s => {
                merged += 1;
                let sd = &func.sized_stack_slots[s];
                grown.push((r, sd.size, sd.align_shift));
                func.sized_stack_slots[s].size = 0;
                // `-g`: `r`'s bytes now hold `s`'s data part of the time,
                // so it no longer locates `r`'s variable.
                func.sized_stack_slots[r].key = None;
            }
            _ => {}
        }
    }
    if crate::env_set("PLIRON_SM_INFO") {
        let (mut nz, mut bytes) = (0u32, 0u64);
        let mut cat: FxHashMap<&'static str, (u32, u64)> = FxHashMap::default();
        for s in 0..nslots {
            let s = StackSlot::from_u32(s as u32);
            let sz = func.sized_stack_slots[s].size as u64;
            if sz == 0 {
                continue;
            }
            nz += 1;
            bytes += sz;
            let k = if escaped[s.index()] {
                "escaped"
            } else if rep.get(&s) == Some(&s) && iv[s.index()].0 != u32::MAX {
                "region-rep"
            } else if iv[s.index()].0 == u32::MAX {
                "unused-referenced"
            } else {
                "other"
            };
            let e = cat.entry(k).or_default();
            e.0 += 1;
            e.1 += sz;
        }
        eprintln!("sm-info: post: {nz} nonzero slots {bytes}B by-kind {cat:?}");
    }
    if merged > 0 {
        let insts: Vec<Inst> = order
            .iter()
            .flat_map(|&b| func.layout.block_insts(b))
            .collect();
        for i in insts {
            if let InstructionData::StackAddr { stack_slot, .. } = &mut func.dfg.insts[i]
                && let Some(&r) = rep.get(stack_slot)
            {
                *stack_slot = r;
            }
        }
        for (r, size, align) in grown {
            let rd = &mut func.sized_stack_slots[r];
            rd.size = rd.size.max(size);
            rd.align_shift = rd.align_shift.max(align);
        }
    }
    merged
}

/// Bytes of each slot `poison_dead` overwrites (the rest stay as they are:
/// a wrong interval still trips on the first 256 bytes in practice).
const POISON_MAX: u32 = 256;

/// See `PLIRON_SM_POISON` in `run`. A slot's interval `[lo, hi]` is in the
/// same RPO positions coloring uses; outside it the bytes are claimed
/// dead. Poison right after the instruction at `hi` (unless it ends its
/// block) and at the head of every block lying wholly outside the
/// interval that is entered from a block overlapping it: any path that
/// leaves the live range and comes back without rewriting the slot then
/// reads 0xA5..., exactly what a merged neighbour's write could have left.
fn poison_dead(
    func: &mut Function,
    order: &[Block],
    bfirst: &SecondaryMap<Block, u32>,
    blast: &SecondaryMap<Block, u32>,
    succ_map: &SecondaryMap<Block, Vec<Block>>,
    iv: &[(u32, u32)],
    escaped: &[bool],
) -> usize {
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::{InstBuilder, types};
    let mut pos_inst: Vec<Inst> = Vec::new();
    for &b in order {
        pos_inst.extend(func.layout.block_insts(b));
    }
    let overlaps = |b: Block, lo: u32, hi: u32| bfirst[b] <= hi && blast[b] >= lo;
    // `PLIRON_SM_POISON_SLOTS=a..b`: poison only slot indices in [a, b)
    // (bisect which slot's interval is wrong).
    let range: Option<(usize, usize)> = crate::env_get("PLIRON_SM_POISON_SLOTS").and_then(|r| {
        let (a, b) = r.split_once("..")?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    let mut sites: Vec<(StackSlot, Option<Block>, Option<Inst>)> = Vec::new();
    for s in 0..iv.len() {
        let (lo, hi) = iv[s];
        if lo == u32::MAX || escaped[s] || range.is_some_and(|(a, b)| s < a || s >= b) {
            continue;
        }
        let ss = StackSlot::from_u32(s as u32);
        if crate::env_set("PLIRON_SM_INFO") {
            eprintln!("sm-info: poison {ss} iv=({lo}, {hi}) at-hi={:?}", pos_inst.get(hi as usize));
        }
        if let Some(&i) = pos_inst.get(hi as usize)
            && !func.dfg.insts[i].opcode().is_terminator()
        {
            sites.push((ss, None, Some(i)));
        }
        let mut heads: FxHashSet<Block> = FxHashSet::default();
        for &b in order {
            if !overlaps(b, lo, hi) {
                continue;
            }
            for &t in &succ_map[b] {
                if !overlaps(t, lo, hi) {
                    heads.insert(t);
                }
            }
        }
        sites.extend(heads.into_iter().map(|t| (ss, Some(t), None)));
    }
    // `PLIRON_SM_POISON_SITES=a..b`: keep only sites a..b (in the order
    // `PLIRON_SM_INFO` lists them) — bisect which exit path is wrong.
    if let Some((a, b)) = crate::env_get("PLIRON_SM_POISON_SITES").and_then(|r| {
        let (a, b) = r.split_once("..")?;
        Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
    }) {
        sites = sites.into_iter().enumerate().filter(|(k, _)| *k >= a && *k < b).map(|(_, x)| x).collect();
    }
    if crate::env_set("PLIRON_SM_INFO") {
        for (k, (ss, head, after)) in sites.iter().enumerate() {
            let span = head.map(|b| (bfirst[b], blast[b]));
            eprintln!("sm-info: poison site {k}: {ss} head={head:?} pos={span:?} after={after:?}");
        }
    }
    let n = sites.len();
    // The pointer type `stack_store` needs: whatever this function's
    // `stack_addr`s produce (i32 on wasm32).
    let pty = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .find(|&i| matches!(func.dfg.insts[i], InstructionData::StackAddr { .. }))
        .map(|i| func.dfg.value_type(func.dfg.first_result(i)))
        .unwrap_or(types::I64);
    for (ss, head, after) in sites {
        let size = func.sized_stack_slots[ss].size.min(POISON_MAX);
        let mut cur = FuncCursor::new(func);
        match (head, after) {
            (Some(b), _) => {
                let Some(first) = cur.func.layout.first_inst(b) else {
                    continue;
                };
                cur = cur.at_inst(first);
            }
            (None, Some(i)) => {
                cur = cur.after_inst(i);
            }
            _ => continue,
        }
        let w = cur.ins().iconst(types::I64, 0xA5A5_A5A5_A5A5_A5A5u64 as i64);
        let mut off = 0u32;
        while off + 8 <= size {
            cur.ins().stack_store(pty, w, ss, off as i32);
            off += 8;
        }
        if off < size {
            let b8 = cur.ins().iconst(types::I8, 0xA5);
            while off < size {
                cur.ins().stack_store(pty, b8, ss, off as i32);
                off += 1;
            }
        }
    }
    n
}

/// Does write `w` (a store or a `memset`/`memcpy` libcall recorded in
/// `writes`) overwrite all of `s`? Only then does it make the slot fresh
/// for a loop trip: an 8-byte store to an array iterator's counter left
/// the array bytes behind it live across the back edge (tests/loopvec,
/// caught by `PLIRON_SM_POISON`), so a merge there could clobber them.
fn covers_slot(func: &Function, w: Inst, s: StackSlot) -> bool {
    let size = func.sized_stack_slots[s].size as i64;
    // `v` = `stack_addr s+off` (through aliases): `off`.
    let base_off = |v: Value| -> Option<i64> {
        let v = func.dfg.resolve_aliases(v);
        let i = func.dfg.value_def(v).inst()?;
        match func.dfg.insts[i] {
            InstructionData::StackAddr { stack_slot, offset, .. } if stack_slot == s => {
                Some(i64::from(offset))
            }
            _ => None,
        }
    };
    match func.dfg.insts[w] {
        InstructionData::Store { args, offset, .. } => {
            let bytes = i64::from(func.dfg.value_type(args[0]).bytes());
            base_off(args[1]).is_some_and(|o| o + i64::from(offset) == 0 && bytes >= size)
        }
        _ => {
            // mem* libcall: (dst, src|byte, len).
            let a = func.dfg.inst_args(w);
            let len = a.get(2).and_then(|&l| {
                let i = func.dfg.value_def(func.dfg.resolve_aliases(l)).inst()?;
                match func.dfg.insts[i] {
                    InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
                    _ => None,
                }
            });
            a.first().and_then(|&d| base_off(d)) == Some(0) && len.is_some_and(|n| n >= size)
        }
    }
}
