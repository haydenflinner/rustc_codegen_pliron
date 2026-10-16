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

use cranelift_codegen::entity::{EntityRef, SecondaryMap};
use cranelift_codegen::ir::{
    ArgumentPurpose, Block, BlockArg, FuncRef, Function, Inst, InstructionData, Opcode, StackSlot,
    Value, instructions::CallInfo,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

// Worklist propagation of a slot-set map over a function: branch args
// flow into destination params (try_call payloads inherit the call
// args' union); every other inst's results take the union of its args'
// sets. `wl`/`inq` are the caller's initial worklist.
fn propagate(
    func: &Function,
    users: &SecondaryMap<Value, Vec<Inst>>,
    map: &mut SecondaryMap<Value, Vec<StackSlot>>,
    mut wl: VecDeque<Inst>,
    mut inq: FxHashSet<Inst>,
    contains: Option<&Vec<FxHashSet<StackSlot>>>,
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
                        BlockArg::TryCallRet(_) | BlockArg::TryCallExn(_) => call_slots.clone(),
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

pub fn run(
    func: &mut Function,
    nowrite: &FxHashMap<FuncRef, bool>,
    lifetimes: &[(Value, Option<Inst>, Block, bool)],
    nocap: &FxHashSet<(Inst, u32)>,
) -> usize {
    if func.sized_stack_slots.len() < 2 {
        return 0;
    }

    // Intervals are measured in instruction positions (layout order), so
    // slots used at disjoint points inside one block can still merge.
    // `bpos`/`bfirst`/`blast` give each block's position range.
    let mut bpos = SecondaryMap::<Block, u32>::new();
    let mut bfirst = SecondaryMap::<Block, u32>::new();
    let mut blast = SecondaryMap::<Block, u32>::new();
    let mut ipos = SecondaryMap::<Inst, u32>::new();
    let mut order: Vec<Block> = Vec::new();
    let mut next = 0u32;
    for (i, b) in func.layout.blocks().enumerate() {
        bpos[b] = i as u32;
        order.push(b);
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
    propagate(func, &users, &mut derived, wl, inq, None);

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
    propagate(func, &users, &mut from_bytes, wl, inq, None);

    // Per-slot use positions and live interval [lo, hi] (`u32::MAX` hi is
    // normalized to `last` afterwards).
    let nslots = func.sized_stack_slots.len();
    let mut iv: Vec<(u32, u32)> = vec![(u32::MAX, 0); nslots];
    let mut uses: Vec<Vec<u32>> = vec![Vec::new(); nslots];
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
    let mut marked = vec![false; nslots];
    if crate::env_set("PLIRON_SLOTMERGE_MARK") {
        for &(v, _, _, _) in lifetimes {
            let ds = &derived[func.dfg.resolve_aliases(v)];
            // A marker operand naming exactly one slot is that storage's
            // window; a unioned (phi-derived) operand can't be trusted as a
            // per-slot bound, so ambiguous markers mark nothing.
            if let &[s] = ds.as_slice()
                && s.index() < nslots
            {
                marked[s.index()] = true;
            }
        }
    }
    macro_rules! escape {
        ($s:expr, $i:expr) => {{
            if !marked[$s.index()] {
                iv[$s.index()].1 = u32::MAX;
                if dbg {
                    esc.push(($s, $i));
                }
            }
        }};
    }
    // Escaping a pointer to `s` exposes `s`'s bytes — including pointers
    // stored inside it — to whoever holds the escapee. Escape the whole
    // contains-reach.
    macro_rules! escape_deep {
        ($c:expr, $s:expr, $i:expr) => {{
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
    // d itself, every slot whose pointer was stored into d, and the
    // closure through byte-copies.
    let mut contains: Vec<FxHashSet<StackSlot>> = (0..nslots)
        .map(|s| [StackSlot::from_u32(s as u32)].into_iter().collect())
        .collect();

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
                    let info =
                        d.analyze_call(&func.dfg.value_lists, &func.dfg.exception_tables);
                    if let CallInfo::Direct(fr, cargs)
                    | CallInfo::DirectWithSig(fr, _, cargs) = info
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
                                    if s != d2 {
                                        grew += edges.insert((s, d2)) as usize;
                                    }
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
                                        if s != d2 {
                                            grew += edges.insert((s, d2)) as usize;
                                        }
                                    }
                                }
                                for &s in &from_bytes[func.dfg.resolve_aliases(args[k])]
                                {
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
                let mut acc: Vec<StackSlot> = Vec::new();
                for &s0 in &derived[func.dfg.resolve_aliases(a)] {
                    if !acc.contains(&s0) {
                        acc.push(s0);
                    }
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
        propagate(func, &users, &mut derived, wl2, inq2, Some(&contains));
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
                                    let nokeep = matches!(
                                        func.dfg.ext_funcs[fr].name,
                                        ExternalName::LibCall(LibCall::Memcpy)
                                            | ExternalName::LibCall(LibCall::Memmove)
                                            | ExternalName::LibCall(LibCall::Memset)
                                    );
                                    for (k, &a) in cargs.iter().enumerate() {
                                        for &s in &derived[func.dfg.resolve_aliases(a)] {
                                            use_at(&mut iv, &mut uses, s, pos);
                                            // rustc's captures(none): the
                                            // callee can't retain the
                                            // pointer past the call.
                                            if !nw
                                                && !nokeep
                                                && !(sret && k == 0)
                                                && !nocap.contains(&(i, k as u32))
                                            {
                                                escape_deep!(contains, s, i);
                                            }
                                        }
                                    }
                                }
                                CallInfo::Indirect(_, cargs) => {
                                    for (k, &a) in cargs.iter().enumerate() {
                                        for &s in &derived[func.dfg.resolve_aliases(a)] {
                                            use_at(&mut iv, &mut uses, s, pos);
                                            if !nocap.contains(&(i, k as u32)) {
                                                escape_deep!(contains, s, i);
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
                            for &s in arg_slots(1) {
                                use_at(&mut iv, &mut uses, s, pos);
                            }
                        }
                        // atomic_rmw p, v / atomic_cas p, e, n
                        Opcode::AtomicRmw | Opcode::AtomicCas => {
                            let dst: Vec<StackSlot> = arg_slots(0).to_vec();
                            for &s in &dst {
                                use_at(&mut iv, &mut uses, s, pos);
                            }
                            for k in 1..args.len() {
                                for &s in arg_slots(k) {
                                    use_at(&mut iv, &mut uses, s, pos);
                                    if dst.is_empty() {
                                        escape_deep!(contains, s, i);
                                    }
                                }
                                if dst.is_empty() {
                                    for &s in &from_bytes[func.dfg.resolve_aliases(args[k])]
                                    {
                                        escape_deep!(contains, s, i);
                                    }
                                }
                            }
                        }
                        _ => {
                            for k in 0..args.len() {
                                for &s in arg_slots(k) {
                                    use_at(&mut iv, &mut uses, s, pos);
                                    escape!(s, i);
                                }
                            }
                        }
                    }
                } else if op.can_load() {
                    for &s in arg_slots(0) {
                        use_at(&mut iv, &mut uses, s, pos);
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
    // Marker positions count as uses too: they bound the validity window
    // even where the scan saw no byte-touches (e.g. a slot only ever
    // passed by reference). Each marker sits right after the anchor
    // instruction recorded at lowering; if a pass deleted the anchor,
    // fall back to the block's start, and if the block itself left the
    // layout drop the marker — real byte-uses still bound the interval.
    let in_layout: FxHashSet<Block> = order.iter().copied().collect();
    for &(v, anchor, b, start) in lifetimes {
        let pos = match anchor {
            Some(i) if func.layout.inst_block(i).is_some() => {
                // The marker sits between `i` and the next inst: a start
                // marker is live from the next position, an end marker's
                // last live point is `i` itself.
                if start {
                    (ipos[i] + 1).min(last)
                } else {
                    ipos[i]
                }
            }
            _ if in_layout.contains(&b) => bfirst[b],
            _ => continue,
        };
        for &s in &derived[func.dfg.resolve_aliases(v)] {
            if s.index() < nslots {
                use_at(&mut iv, &mut uses, s, pos);
            }
        }
    }

    for e in iv.iter_mut() {
        if e.1 == u32::MAX && e.0 != u32::MAX {
            e.1 = last;
        }
    }

    // A backward edge lets a path re-enter an earlier region, so any slot
    // used inside the edge's layout span stays live across the whole span.
    let mut spans: Vec<(u32, u32)> = Vec::new();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                let v = bc.block(&func.dfg.value_lists);
                if bpos[v] < bpos[b] {
                    spans.push((bfirst[v], blast[b]));
                }
            }
        }
    }
    for (s, e) in iv.iter_mut().enumerate() {
        if marked[s] {
            continue;
        }
        for &(lo, hi) in &spans {
            if uses[s].iter().any(|&p| lo <= p && p <= hi) {
                e.0 = e.0.min(lo);
                e.1 = e.1.max(hi);
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
    // Greedy interval coloring, best-fit by region size.
    let mut cand: Vec<(u32, StackSlot)> = (0..nslots)
        .map(|s| StackSlot::from_u32(s as u32))
        .filter(|&s| iv[s.index()].0 != u32::MAX)
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
            }
            _ => {}
        }
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
