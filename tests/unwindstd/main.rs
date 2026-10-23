// std unwind-safety guards under a panic at each of 12 points: Vec extend
// / to_vec with a panicking Clone, sort / sort_unstable / BTreeMap /
// BTreeSet / BinaryHeap with a panicking Ord, RefCell flags, drop order,
// collect + Rc counts, VecDeque / HashMap / String / retain / dedup_by.
// Output must match stock LLVM rustc (expected.out). Companion to
// tests/unwindmem (dse/licm unwind exits, 2026-10-22).
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, VecDeque};
use std::hint::black_box;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

thread_local! { static BOOM: Cell<u64> = Cell::new(u64::MAX); static TICK: Cell<u64> = Cell::new(0); }
fn tick() -> u64 { TICK.with(|t| { let n = t.get(); t.set(n + 1); if n == BOOM.with(|b| b.get()) { panic!("boom at {n}") } n }) }
fn arm(at: u64) { TICK.with(|t| t.set(0)); BOOM.with(|b| b.set(at)); }

#[derive(Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct P(u64);
impl Clone for P { fn clone(&self) -> Self { tick(); P(self.0) } }

#[derive(Debug, Eq, PartialEq)]
struct K(u64);
impl Ord for K { fn cmp(&self, o: &Self) -> std::cmp::Ordering { tick(); self.0.cmp(&o.0) } }
impl PartialOrd for K { fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) } }

struct D<'a>(u64, &'a RefCell<Vec<u64>>);
impl Drop for D<'_> { fn drop(&mut self) { self.1.borrow_mut().push(self.0); } }

fn run(at: u64) -> String {
    let mut out = Vec::new();
    // Vec::extend_from_slice with a panicking Clone: len must count the clones made.
    arm(at); let src: Vec<P> = (0..12).map(P).collect(); let mut v: Vec<P> = vec![P(100)];
    let r = catch_unwind(AssertUnwindSafe(|| v.extend_from_slice(black_box(&src)))).is_ok();
    out.push(format!("ext {r} {}", v.len()));
    // Vec::clone / to_vec
    arm(at); let r = catch_unwind(AssertUnwindSafe(|| black_box(&src).to_vec().len())).ok(); out.push(format!("tovec {r:?}"));
    // sort with panicking comparator: must stay a permutation.
    arm(at); let mut ks: Vec<K> = (0..40).map(|i| K((i * 37) % 41)).collect();
    let r = catch_unwind(AssertUnwindSafe(|| ks.sort())).is_ok();
    let mut ids: Vec<u64> = ks.iter().map(|k| k.0).collect(); ids.sort_unstable(); ids.dedup();
    out.push(format!("sort {r} perm={}", ids.len()));
    arm(at); let mut ks: Vec<K> = (0..40).map(|i| K((i * 37) % 41)).collect();
    let r = catch_unwind(AssertUnwindSafe(|| ks.sort_unstable())).is_ok();
    let mut ids: Vec<u64> = ks.iter().map(|k| k.0).collect(); ids.sort_unstable(); ids.dedup();
    out.push(format!("usort {r} perm={}", ids.len()));
    // BTreeMap / BTreeSet insert with panicking Ord.
    arm(at); let mut bm = BTreeMap::new();
    let r = catch_unwind(AssertUnwindSafe(|| for i in 0..30 { bm.insert(K(i * 7 % 31), i); })).is_ok();
    out.push(format!("btree {r} {} {:?}", bm.len(), bm.keys().map(|k| k.0).sum::<u64>()));
    arm(at); let mut bs = BTreeSet::new();
    let r = catch_unwind(AssertUnwindSafe(|| for i in 0..30 { bs.insert(K(i * 11 % 31)); })).is_ok();
    out.push(format!("bset {r} {}", bs.len()));
    // BinaryHeap push with panicking Ord.
    arm(at); let mut bh = BinaryHeap::new();
    let r = catch_unwind(AssertUnwindSafe(|| for i in 0..20 { bh.push(K(i * 13 % 23)); })).is_ok();
    out.push(format!("heap {r} {}", bh.len()));
    // RefCell borrow flag restored on unwind.
    arm(at); let rc = RefCell::new(5u64);
    let r = catch_unwind(AssertUnwindSafe(|| { let mut b = rc.borrow_mut(); *b += 1; tick(); tick(); *b += 1; })).is_ok();
    let free = rc.try_borrow_mut().is_ok();
    let val = *rc.borrow();
    out.push(format!("refcell {r} {free} {val}"));
    // Drop order of locals during unwind.
    arm(at); let log = RefCell::new(Vec::new());
    let r = catch_unwind(AssertUnwindSafe(|| { let _a = D(1, &log); tick(); let _b = D(2, &log); tick(); let _c = D(3, &log); tick(); })).is_ok();
    out.push(format!("drops {r} {:?}", log.borrow()));
    // collect from a panicking map: partial Vec dropped, Rc counts restored.
    arm(at); let rcv = Rc::new(7u64);
    let r = catch_unwind(AssertUnwindSafe(|| (0..10).map(|_| { tick(); rcv.clone() }).collect::<Vec<_>>().len())).ok();
    out.push(format!("collect {r:?} rc={}", Rc::strong_count(&rcv)));
    // VecDeque extend + HashMap with panicking Clone values.
    arm(at); let mut dq: VecDeque<P> = VecDeque::new();
    let r = catch_unwind(AssertUnwindSafe(|| dq.extend(src.iter().cloned()))).is_ok();
    out.push(format!("deque {r} {}", dq.len()));
    arm(at); let mut hm: HashMap<u64, P> = HashMap::new();
    let r = catch_unwind(AssertUnwindSafe(|| for p in &src { hm.insert(p.0, p.clone()); })).is_ok();
    out.push(format!("hmap {r} {}", hm.len()));
    // String push in a loop with panic.
    arm(at); let mut s = String::new();
    let r = catch_unwind(AssertUnwindSafe(|| for i in 0..10 { s.push(char::from(b'a' + i as u8)); tick(); })).is_ok();
    out.push(format!("str {r} {s}"));
    // retain with panicking predicate: Vec stays consistent.
    arm(at); let mut rv: Vec<u64> = (0..20).collect();
    let r = catch_unwind(AssertUnwindSafe(|| rv.retain(|x| { tick(); x % 3 != 0 }))).is_ok();
    out.push(format!("retain {r} {} {}", rv.len(), rv.iter().sum::<u64>()));
    // dedup_by with panicking closure
    arm(at); let mut dv: Vec<u64> = vec![1, 1, 2, 2, 2, 3, 4, 4, 5, 5, 5, 5, 6];
    let r = catch_unwind(AssertUnwindSafe(|| dv.dedup_by(|a, b| { tick(); a == b }))).is_ok();
    out.push(format!("dedup {r} {:?}", dv));
    out.join(" | ")
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    for at in [0u64, 1, 2, 3, 5, 8, 13, 21, 34, 55, 89, u64::MAX] { println!("at={at}: {}", run(at)); }
}
