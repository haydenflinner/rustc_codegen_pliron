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
            for bc in func.dfg.insts[i].branch_destination(
                &func.dfg.jump_tables,
                &func.dfg.exception_tables,
            ) {
                for ba in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = ba {
                        users[func.dfg.resolve_aliases(v)].push(i);
                    }
                }
            }
        }
    }

    // `derived[v]` = slots whose address `v` may hold. Seeds are
    // `stack_addr` results; results copy the union of their args' sets
    // (over-approximating only costs merges); branch args flow into the
    // destination's block params, and a `try_call`'s return / exception
    // payloads conservatively inherit the union of its call args.
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
    while let Some(i) = wl.pop_front() {
        inq.remove(&i);
        if func.dfg.insts[i].opcode().is_branch() {
            let call_slots: Vec<StackSlot> = if func.dfg.insts[i].opcode().is_call() {
                let mut v = Vec::new();
                for &a in func.dfg.inst_args(i) {
                    for &s in &derived[func.dfg.resolve_aliases(a)] {
                        if !v.contains(&s) {
                            v.push(s);
                        }
                    }
                }
                v
            } else {
                Vec::new()
            };
            for bc in func.dfg.insts[i].branch_destination(
                &func.dfg.jump_tables,
                &func.dfg.exception_tables,
            ) {
                let blk = bc.block(&func.dfg.value_lists);
                for (k, ba) in bc.args(&func.dfg.value_lists).enumerate() {
                    let src: Vec<StackSlot> = match ba {
                        BlockArg::Value(v) => derived[func.dfg.resolve_aliases(v)].clone(),
                        BlockArg::TryCallRet(_) | BlockArg::TryCallExn(_) => call_slots.clone(),
                    };
                    if src.is_empty() {
                        continue;
                    }
                    let Some(&p) = func.dfg.block_params(blk).get(k) else {
                        continue;
                    };
                    // A non-scalar-int param can't hold a pointer.
                    let ty = func.dfg.value_type(p);
                    if ty.is_vector() || !ty.is_int() {
                        continue;
                    }
                    let mut grew = false;
                    for &s in &src {
                        if !derived[p].contains(&s) {
                            derived[p].push(s);
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
            for &s in &derived[func.dfg.resolve_aliases(a)] {
                if !acc.contains(&s) {
                    acc.push(s);
                }
            }
        }
        if acc.is_empty() {
            continue;
        }
        // Loaded data can't carry a slot pointer unless the load is
        // scalar-integer shaped: vector/float loads return bytes, not
        // pointers, so they can't make the slot escape when stored onward
        // — this keeps chunked i8x16 aggregate copies from pinning every
        // slot to the end. All other insts propagate into every result.
        let loadish = func.dfg.insts[i].opcode().can_load();
        for &r in func.dfg.inst_results(i) {
            let r = func.dfg.resolve_aliases(r);
            if loadish {
                let ty = func.dfg.value_type(r);
                if ty.is_vector() || !ty.is_int() {
                    continue;
                }
            }
            let mut grew = false;
            for &s in &acc {
                if !derived[r].contains(&s) {
                    derived[r].push(s);
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

    // Per-slot use positions and live interval [lo, hi] (`u32::MAX` hi is
    // normalized to `last` afterwards).
    let nslots = func.sized_stack_slots.len();
    let mut iv: Vec<(u32, u32)> = vec![(u32::MAX, 0); nslots];
    let mut uses: Vec<Vec<u32>> = vec![Vec::new(); nslots];
    let dbg = crate::env_set("PLIRON_SLOTMERGE_DEBUG");
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
    // (stored, container): `&stored` was written into `container`'s bytes.
    // A reload through the container can expose the pointer at any point
    // in the container's live range, so the stored slot's interval must
    // contain the container's — still bounded when the container is.
    let mut edges: FxHashSet<(StackSlot, StackSlot)> = FxHashSet::default();
    // (src, dst): memcpy/memmove copied src's bytes into dst, so pointers
    // contained in src are now contained in dst as well.
    let mut cpy: Vec<(StackSlot, StackSlot)> = Vec::new();
    let use_at = |iv: &mut Vec<(u32, u32)>,
                      uses: &mut Vec<Vec<u32>>,
                      s: StackSlot,
                      p: u32| {
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
                        Opcode::ReturnCall
                        | Opcode::ReturnCallIndirect
                        | Opcode::StackSwitch => {
                            for k in 0..args.len() {
                                for &s in arg_slots(k) {
                                    use_at(&mut iv, &mut uses, s, pos);
                                    escape!(s, i);
                                }
                            }
                        }
                        _ => {
                            let info = d.analyze_call(
                                &func.dfg.value_lists,
                                &func.dfg.exception_tables,
                            );
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
                                    // but never retains them. memcpy/
                                    // memmove also duplicate the src
                                    // region's contents into dst, so
                                    // contained pointers flow dst-ward.
                                    use cranelift_codegen::ir::{ExternalName, LibCall};
                                    let (nokeep, copy) = match func.dfg.ext_funcs[fr].name {
                                        ExternalName::LibCall(LibCall::Memcpy)
                                        | ExternalName::LibCall(LibCall::Memmove) => (true, true),
                                        ExternalName::LibCall(LibCall::Memset) => (true, false),
                                        _ => (false, false),
                                    };
                                    if copy && cargs.len() >= 2 {
                                        for &x in &derived[func.dfg.resolve_aliases(cargs[1])] {
                                            for &d in
                                                &derived[func.dfg.resolve_aliases(cargs[0])]
                                            {
                                                cpy.push((x, d));
                                            }
                                        }
                                    }
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
                                                escape!(s, i);
                                            }
                                        }
                                    }
                                }
                                CallInfo::Indirect(_, cargs) => {
                                    for (k, &a) in cargs.iter().enumerate() {
                                        for &s in &derived[func.dfg.resolve_aliases(a)] {
                                            use_at(&mut iv, &mut uses, s, pos);
                                            if !nocap.contains(&(i, k as u32)) {
                                                escape!(s, i);
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
                                    escape!(s, i);
                                } else {
                                    for &d in &dst {
                                        if d != s {
                                            edges.insert((s, d));
                                        }
                                    }
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
                                        escape!(s, i);
                                    } else {
                                        for &d in &dst {
                                            if d != s {
                                                edges.insert((s, d));
                                            }
                                        }
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
                            escape!(s, i);
                        }
                    }
                } else if op.other_side_effects() || op.is_terminator() {
                    for k in 0..args.len() {
                        for &s in arg_slots(k) {
                            use_at(&mut iv, &mut uses, s, pos);
                            escape!(s, i);
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
                if start { (ipos[i] + 1).min(last) } else { ipos[i] }
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
            for bc in func.dfg.insts[i].branch_destination(
                &func.dfg.jump_tables,
                &func.dfg.exception_tables,
            ) {
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

    // Container edges: `&s` stored into `d`'s bytes stays reachable for
    // `d`'s whole live range, so `s`'s interval must contain `d`'s.
    // memcpy/memmove copy edges duplicate contained pointers dst-ward.
    // Iterate to a fixpoint — chains and cycles both converge.
    loop {
        let mut changed = false;
        for &(x, d) in &cpy {
            let add: Vec<StackSlot> = edges
                .iter()
                .filter_map(|&(s, c)| (c == x && s != d).then_some(s))
                .collect();
            for s in add {
                changed |= edges.insert((s, d));
            }
        }
        for &(s, d) in &edges {
            let (dlo, dhi) = iv[d.index()];
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
        if !changed {
            break;
        }
    }

    if dbg {
        for s in 0..nslots {
            let s = StackSlot::from_u32(s as u32);
            eprintln!(
                "slotmerge: {s} iv={:?} uses={:?}",
                iv[s.index()],
                uses[s.index()],
            );
        }
        for &(s, i) in &esc {
            eprintln!(
                "slotmerge: {s} escapes at {i} {}: {}",
                func.layout.inst_block(i).unwrap(),
                func.dfg.display_inst(i),
            );
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
            if end < lo && rsize >= size && ralign >= align && best.is_none_or(|b| regions[b].1 > rsize)
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
    // interval (no `stack_addr` references at all) are just zeroed.
    let mut merged = 0;
    let mut grown: Vec<(StackSlot, u32, u8)> = Vec::new();
    for s in 0..nslots {
        let s = StackSlot::from_u32(s as u32);
        match rep.get(&s).copied() {
            None => {
                if iv[s.index()].0 == u32::MAX {
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
