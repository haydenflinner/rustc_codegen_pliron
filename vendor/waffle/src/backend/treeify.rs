//! Treeification: placing some values "under" others if only used
//! once, to generate more AST-like Wasm code.

use crate::cfg::CFGInfo;
use crate::entity::EntityRef;
use crate::ir::{Block, FunctionBody, Value, ValueDef};
use crate::Operator;
use fxhash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::convert::TryFrom;

/// One "argument slot" of an operator defining a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueArg(Value, u16);

impl ValueArg {
    /// PLIRON: the value whose arg slot this is — needed by
    /// localify's steal analysis to walk a treeified chain to its
    /// consuming root.
    pub fn root_value(&self) -> Value {
        self.0
    }
}

#[derive(Clone, Debug)]
pub struct Trees {
    /// Is a value placed "under" the given arg slot of the given
    /// other value?
    pub owner: HashMap<Value, ValueArg>,
    /// For a given value that is defined by an operator, which
    /// Values, if any, live at each slot?
    pub owned: HashMap<ValueArg, Value>,
    /// Values that are regenerated every time they are used.
    pub remat: HashSet<Value>,
}

fn is_remat(op: &Operator) -> bool {
    // Only ops with no args can be always-rematerialized.
    match op {
        Operator::I32Const { .. }
        | Operator::I64Const { .. }
        | Operator::F32Const { .. }
        | Operator::F64Const { .. } => true,
        _ => false,
    }
}

/// PLIRON: unary ops cheap enough that rematting ≤2 uses beats a local.
fn is_cheap_unary(op: &Operator) -> bool {
    matches!(
        op,
        Operator::I32Eqz
            | Operator::I64Eqz
            | Operator::I32WrapI64
            | Operator::I64ExtendI32U
            | Operator::I64ExtendI32S
            | Operator::I32Extend8S
            | Operator::I32Extend16S
            | Operator::I64Extend8S
            | Operator::I64Extend16S
            | Operator::I64Extend32S
            | Operator::I32Clz
            | Operator::I32Ctz
            | Operator::I32Popcnt
            | Operator::I64Clz
            | Operator::I64Ctz
            | Operator::I64Popcnt
    )
}

impl Trees {
    pub fn compute(body: &FunctionBody, cfg: &CFGInfo) -> Trees {
        let mut owner = HashMap::default();
        let mut owned = HashMap::default();
        let mut remat = HashSet::default();
        let mut multi_use = HashSet::default();
        // PLIRON: read once per pass.
        let loads_ok = !std::env::var_os("PLIRON_WTREELOAD").is_some_and(|v| v == "0");
        // PLIRON: keep a tree from pulling a def into a loop it sits
        // outside of — a single-use invariant licm hoisted would otherwise
        // be re-emitted at its in-loop use. `PLIRON_WTREE_LOOP=0` off.
        let nest = (!std::env::var_os("PLIRON_WTREE_LOOP").is_some_and(|v| v == "0"))
            .then(|| LoopNest::compute(cfg));

        for (block, block_def) in body.blocks.entries() {
            for &value in &block_def.insts {
                match &body.values[value] {
                    &ValueDef::Operator(op, args, _) => {
                        // Ignore operators with invalid args: these must
                        // always be unreachable.
                        if body.arg_pool[args].iter().any(|arg| arg.is_invalid()) {
                            continue;
                        }
                        // If this is an always-rematerialized operator,
                        // mark it as such and continue.
                        if is_remat(&op) {
                            remat.insert(value);
                            continue;
                        }

                        // For each of the args, if the value is produced
                        // by a single-output op and is movable, and is
                        // not already recorded in `multi_use`, place it
                        // in the arg slot. Otherwise if owned already
                        // somewhere else, undo that and put in
                        // `multi_use`.
                        for (i, &arg) in body.arg_pool[args].iter().enumerate() {
                            let arg = body.resolve_alias(arg);
                            if multi_use.contains(&arg) {
                                continue;
                            } else if let Some(old_owner) = owner.remove(&arg) {
                                owned.remove(&old_owner);
                                multi_use.insert(arg);
                            } else if Self::is_movable(body, arg, loads_ok) {
                                // A use this tree can't serve still counts:
                                // `multi_use` stops a later use owning it.
                                if nest
                                    .as_ref()
                                    .is_some_and(|n| !n.encloses(cfg.def_block[arg], block))
                                {
                                    multi_use.insert(arg);
                                    continue;
                                }
                                let pos = u16::try_from(i).unwrap();
                                let value_arg = ValueArg(value, pos);
                                owner.insert(arg, value_arg);
                                owned.insert(value_arg, arg);
                            }
                        }
                    }
                    &ValueDef::PickOutput(..) => {
                        // Can ignore use: multi-arity values are never treeified.
                    }
                    &ValueDef::BlockParam(..)
                    | &ValueDef::Alias(..)
                    | &ValueDef::Placeholder(..)
                    | &ValueDef::None => {}
                }
            }
        }
        for block in body.blocks.values() {
            block.terminator.visit_uses(|u| {
                let u = body.resolve_alias(u);
                if let Some(old_owner) = owner.remove(&u) {
                    owned.remove(&old_owner);
                }
            });
        }

        // PLIRON: pure values used *only* as terminator/edge args
        // (ownership was purged above) rematerialize at each edge
        // instead of `local.set` + `local.get`s — LLVM's shape for
        // throwaway arithmetic on branches. Inst-uses keep the local.
        // `PLIRON_WTREEREMAT=0` disables.
        if !std::env::var_os("PLIRON_WTREEREMAT").is_some_and(|v| v == "0") {
            let mut inst_uses = vec![0u32; body.values.len()];
            let mut term_uses = vec![0u32; body.values.len()];
            for block in body.blocks.values() {
                for &iv in &block.insts {
                    if let &ValueDef::Operator(_, args, _) = &body.values[iv] {
                        for &a in &body.arg_pool[args] {
                            let a = body.resolve_alias(a);
                            if let Some(slot) = inst_uses.get_mut(a.index()) {
                                *slot = slot.saturating_add(1);
                            }
                        }
                    }
                }
                block.terminator.visit_uses(|u| {
                    let u = body.resolve_alias(u);
                    if let Some(slot) = term_uses.get_mut(u.index()) {
                        *slot = slot.saturating_add(1);
                    }
                });
            }
            for (v, def) in body.values.entries() {
                if remat.contains(&v) || owner.contains_key(&v) {
                    continue;
                }
                let iu = inst_uses.get(v.index()).copied().unwrap_or(0);
                let tu = term_uses.get(v.index()).copied().unwrap_or(0);
                let total = iu.saturating_add(tu);
                if let &ValueDef::Operator(op, _, tys) = def {
                    if tys.len() != 1 || !op.is_pure() {
                        continue;
                    }
                    // Single edge-only use: always remat.
                    if iu == 0 && tu == 1 {
                        remat.insert(v);
                        continue;
                    }
                    // Cheap unary, ≤2 uses total: remat beats a local.
                    // `PLIRON_WTREEREMAT_UNARY=0` disables this arm.
                    if total > 0
                        && total <= 2
                        && is_cheap_unary(&op)
                        && !std::env::var_os("PLIRON_WTREEREMAT_UNARY")
                            .is_some_and(|x| x == "0")
                    {
                        remat.insert(v);
                    }
                }
            }
        }

        // PLIRON: validate load sinking. `is_movable` admits loads so a
        // single-use load can sit inside its consumer's operand tree,
        // emitting at the ultimate root's position instead of a
        // `local.set`/`local.get` round trip — the shape LLVM's wasm
        // backend produces. That moves the load later in program
        // order, so it is sound only when every inst strictly between
        // the def and the consuming root is pure or another load:
        // pure ops neither observe nor clobber memory (and cannot
        // trap, so they cannot change which trap fires first), and
        // reordering wasm loads is unobservable (loads don't write;
        // OOB traps are indistinguishable). Stores, calls, fences,
        // divides, and any other effect block the sink. The root must
        // also be in the def's own block: sinking onto a successor's
        // path would execute the load on edges that never reached it.
        if loads_ok {
            Self::validate_load_sinks(body, &mut owner, &mut owned);
        }

        Trees {
            owner,
            owned,
            remat,
        }
    }

    /// PLIRON: drop owned loads whose consumer tree would emit them in
    /// a different block or past an effectful inst.
    fn validate_load_sinks(
        body: &FunctionBody,
        owner: &mut HashMap<Value, ValueArg>,
        owned: &mut HashMap<ValueArg, Value>,
    ) {
        let is_load = |v: Value| match &body.values[v] {
            &ValueDef::Operator(op, _, _) => op.is_load(),
            _ => false,
        };
        // Owned loads grouped by def block; each block's check uses a
        // prefix count of non-(pure|load) insts so the between-query
        // is O(1) per load.
        let mut per_block: HashMap<crate::ir::Block, Vec<Value>> = HashMap::default();
        let mut orphan = Vec::new();
        for &v in owner.keys() {
            if !is_load(v) {
                continue;
            }
            let db = body.value_blocks[v];
            if db.is_valid() {
                per_block.entry(db).or_default().push(v);
            } else {
                orphan.push(v);
            }
        }
        for v in orphan {
            let slot = owner.remove(&v).unwrap();
            owned.remove(&slot);
        }
        for (db, vs) in per_block {
            // Ultimate (un-owned) root of each owned load, with its
            // inst position. Roots outside `db` fail.
            let insts = &body.blocks[db].insts;
            let mut pos = HashMap::default();
            for (i, &iv) in insts.iter().enumerate() {
                pos.insert(iv, i);
            }
            // bad[i] = count of non-(pure|load) insts in insts[..i).
            let mut bad = Vec::with_capacity(insts.len() + 1);
            bad.push(0usize);
            for &iv in insts.iter() {
                let b = match &body.values[iv] {
                    &ValueDef::Operator(op, _, _) if op.is_pure() || op.is_load() => 0,
                    _ => 1,
                };
                bad.push(bad.last().unwrap() + b);
            }
            let mut unown = Vec::new();
            for v in vs {
                // Walk the owner chain to the emitting root.
                let mut root = owner[&v].0;
                for _ in 0..64 {
                    match owner.get(&root) {
                        Some(&va) => root = va.0,
                        None => break,
                    }
                }
                let (Some(&p), Some(&q)) = (pos.get(&v), pos.get(&root)) else {
                    unown.push(v);
                    continue;
                };
                if p >= q || bad[q] - bad[p + 1] != 0 {
                    unown.push(v);
                }
            }
            for v in unown {
                let slot = owner.remove(&v).unwrap();
                owned.remove(&slot);
            }
        }
    }

    fn is_single_output_op(body: &FunctionBody, value: Value) -> Option<Operator> {
        match &body.values[value] {
            &ValueDef::Operator(op, _, ref tys) if tys.len() == 1 => Some(op),
            _ => None,
        }
    }

    fn is_movable(body: &FunctionBody, value: Value, loads_ok: bool) -> bool {
        Self::is_single_output_op(body, value)
            // PLIRON: loads are tentatively movable; unsafe sinks are
            // reverted by `validate_load_sinks`.
            .map(|op| op.is_pure() || (loads_ok && op.is_load()))
            .unwrap_or(false)
    }
}

/// PLIRON: natural-loop membership per block — the ids of every loop
/// containing it (headers included).
struct LoopNest {
    loops: Vec<smallvec::SmallVec<[u32; 4]>>,
}

impl LoopNest {
    fn compute(cfg: &CFGInfo) -> LoopNest {
        let mut loops: Vec<smallvec::SmallVec<[u32; 4]>> = Vec::new();
        let mut stamp: Vec<u32> = Vec::new();
        let mut work: Vec<Block> = Vec::new();
        let mut id = 0u32;
        for &h in cfg.rpo.values() {
            let latches: smallvec::SmallVec<[Block; 2]> = cfg.preds[h]
                .iter()
                .copied()
                .filter(|&p| cfg.rpo_pos[p].is_some() && cfg.dominates(h, p))
                .collect();
            if latches.is_empty() {
                continue;
            }
            id += 1;
            let mark = |b: Block, loops: &mut Vec<smallvec::SmallVec<[u32; 4]>>, stamp: &mut Vec<u32>| {
                let i = b.index();
                if i >= stamp.len() {
                    stamp.resize(i + 1, 0);
                    loops.resize(i + 1, Default::default());
                }
                if stamp[i] == id {
                    return false;
                }
                stamp[i] = id;
                loops[i].push(id);
                true
            };
            mark(h, &mut loops, &mut stamp);
            work.extend(latches);
            while let Some(b) = work.pop() {
                if mark(b, &mut loops, &mut stamp) {
                    work.extend(
                        cfg.preds[b]
                            .iter()
                            .copied()
                            .filter(|&p| cfg.rpo_pos[p].is_some()),
                    );
                }
            }
        }
        LoopNest { loops }
    }

    /// Every loop containing `u` also contains `d`.
    fn encloses(&self, d: Block, u: Block) -> bool {
        let get = |b: Block| self.loops.get(b.index()).map_or(&[][..], |v| &v[..]);
        let dl = get(d);
        get(u).iter().all(|l| dl.contains(l))
    }
}
