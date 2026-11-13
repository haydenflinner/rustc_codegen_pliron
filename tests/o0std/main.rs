// std's generic code at opt-level 0. The bodies come from std's metadata as
// optimized MIR (inlined callees, storage markers kept) and go through the
// backend's opt-level-0 pipeline, a combination no -O test and no
// user-crate-only opt-level-0 test has. Output is compared with stock
// rustc's.
//
// `HashSet::symmetric_difference` is the regression: its storage-live
// markers were anchored on a `stack_addr` the egraph later moved, slotmerge
// took the first `Difference` for dead while it was waiting to be passed to
// `chain`, and gave its bytes to the second one: `{1,2,3} ^ {2,3,4}` came
// out as `{4}`.
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};
use std::rc::Rc;

fn sorted<T: Ord + Copy>(it: impl Iterator<Item = T>) -> Vec<T> {
    let mut v: Vec<T> = it.collect();
    v.sort();
    v
}

fn sets() {
    let a: HashSet<i32> = [1, 2, 3].into_iter().collect();
    let b: HashSet<i32> = [4, 2, 3, 4].into_iter().collect();
    println!("symdiff {:?}", sorted(a.symmetric_difference(&b).copied()));
    println!("symdiff rev {:?}", sorted(b.symmetric_difference(&a).copied()));
    let mut n = 0;
    for x in a.symmetric_difference(&b) {
        n += *x;
    }
    println!("symdiff loop {n} count {}", a.symmetric_difference(&b).count());
    let d1: HashSet<_> = a.symmetric_difference(&b).collect();
    let d2: HashSet<_> = b.symmetric_difference(&a).collect();
    println!("symdiff eq {}", d1 == d2);
    println!("xor {:?}", sorted((&a ^ &b).into_iter()));
    println!("diff {:?} {:?}", sorted(a.difference(&b).copied()), sorted(b.difference(&a).copied()));
    println!("union {:?}", sorted(a.union(&b).copied()));
    println!("inter {:?}", sorted(a.intersection(&b).copied()));
    println!("or {:?} and {:?} sub {:?}", sorted((&a | &b).into_iter()), sorted((&a & &b).into_iter()), sorted((&a - &b).into_iter()));
    println!("subset {} disjoint {}", a.is_subset(&b), a.is_disjoint(&b));
    let x: BTreeSet<u8> = (0..20).filter(|v| v % 3 == 0).collect();
    let y: BTreeSet<u8> = (0..20).filter(|v| v % 2 == 0).collect();
    println!("bt symdiff {:?}", x.symmetric_difference(&y).collect::<Vec<_>>());
    println!("bt diff {:?} union {:?}", x.difference(&y).collect::<Vec<_>>(), x.union(&y).count());
    println!("bt inter {:?} range {:?}", x.intersection(&y).collect::<Vec<_>>(), x.range(4..=15).collect::<Vec<_>>());
}

fn maps() {
    let mut m: HashMap<String, Vec<u32>> = HashMap::new();
    for (i, w) in "the quick brown fox jumps over the lazy dog the end".split(' ').enumerate() {
        m.entry(w.to_string()).or_default().push(i as u32);
    }
    let mut ks: Vec<(&String, &Vec<u32>)> = m.iter().collect();
    ks.sort();
    println!("map {ks:?}");
    let total: u32 = m.values().flat_map(|v| v.iter()).sum();
    println!("total {total} the={:?} cat={:?}", m.get("the").map(|v| v.len()), m.get("cat"));
    m.retain(|k, v| k.len() > 3 || v.len() > 1);
    println!("retained {:?}", sorted(m.keys().map(|k| k.len())));
    let b: BTreeMap<u32, char> = "hello, world".chars().enumerate().map(|(i, c)| (i as u32 * 7 % 13, c)).collect();
    println!("btree {:?}", b);
    println!("range {:?} first {:?} last {:?}", b.range(3..9).collect::<Vec<_>>(), b.first_key_value(), b.last_key_value());
    let merged: BTreeMap<u32, String> =
        b.iter().zip(b.iter().rev()).map(|((k, a), (_, z))| (*k, format!("{a}{z}"))).collect();
    println!("merged {merged:?}");
}

fn seqs() {
    let mut d: VecDeque<i64> = (1..=6).collect();
    d.rotate_left(2);
    d.push_front(-1);
    let back = d.pop_back();
    println!("deque {d:?} {back:?} {:?}", d.iter().rev().step_by(2).collect::<Vec<_>>());
    let (a, b) = d.as_slices();
    println!("slices {} {}", a.len() + b.len(), d.iter().chain(d.iter().skip(3)).map(|x| x * x).sum::<i64>());
    let mut h: BinaryHeap<(u8, &str)> = [(3, "c"), (9, "z"), (1, "a"), (5, "m")].into_iter().collect();
    h.push((7, "q"));
    let mut out = Vec::new();
    while let Some((p, s)) = h.pop() {
        out.push(format!("{p}{s}"));
    }
    println!("heap {out:?}");
    let v: Vec<u32> = (1..40).collect();
    let w: Vec<u32> = v.windows(3).map(|w| w.iter().product::<u32>() % 97).collect();
    let c: Vec<u32> = v.chunks(7).map(|c| c.iter().sum()).collect();
    println!("windows {:?} chunks {c:?}", &w[..6]);
    let z: Vec<(u32, char)> = v.iter().copied().zip("abcdef".chars().cycle()).skip_while(|p| p.0 < 30).collect();
    println!("zip {z:?}");
    let (ev, od): (Vec<u32>, Vec<u32>) = v.iter().partition(|x| *x % 2 == 0);
    println!("partition {} {} {:?}", ev.len(), od.len(), ev.iter().rev().take(3).chain(od.iter().take(2)).collect::<Vec<_>>());
    let mut s: Vec<String> = "pear apple fig banana kiwi".split_whitespace().map(String::from).collect();
    s.sort_by(|a, b| a.len().cmp(&b.len()).then(b.cmp(a)));
    s.dedup_by_key(|x| x.len());
    println!("sorted {s:?} {:?}", s.iter().map(|x| x.len()).max());
    let pos = v.binary_search(&17).ok().zip(v.iter().position(|&x| x * x > 500));
    println!("search {pos:?} {:?}", v.iter().rposition(|&x| x % 11 == 0));
    let text = "a=1;b=22;c=x;d=4";
    let parsed: Vec<Result<(String, i32), String>> = text
        .split(';')
        .map(|kv| {
            let (k, v) = kv.split_once('=').ok_or("no =")?;
            v.parse::<i32>().map(|n| (k.to_string(), n)).map_err(|e| e.to_string())
        })
        .collect();
    println!("parsed {parsed:?}");
    let all: Result<Vec<i32>, _> = "1 2 3 4".split(' ').map(str::parse::<i32>).collect();
    let none: Option<Vec<u32>> = "1 2 x".split(' ').map(|s| s.parse().ok()).collect();
    println!("collect {all:?} {none:?}");
}

fn shared() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let fs: Vec<Box<dyn Fn(i32) -> i32>> = (1..4)
        .map(|k| {
            let log = Rc::clone(&log);
            Box::new(move |x| {
                log.borrow_mut().push((k, x));
                x * k + k
            }) as Box<dyn Fn(i32) -> i32>
        })
        .collect();
    let r = fs.iter().fold(2, |acc, f| f(acc));
    println!("closures {r} {:?} rc={}", log.borrow(), Rc::strong_count(&log));
    let o: Option<&str> = Some("12");
    let n = o.and_then(|s| s.parse::<u8>().ok()).filter(|n| n % 2 == 0).map_or(0, |n| n + 1);
    let e: Result<u8, String> = Err("bad".into());
    println!("option {n} {:?} {}", e.as_ref().map(|v| v + 1).unwrap_or_else(|s| s.len() as u8), e.is_err());
}

fn main() {
    sets();
    maps();
    seqs();
    shared();
}
