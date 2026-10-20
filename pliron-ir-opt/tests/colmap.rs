//! `pliron::cols::ColMap` sparse-set layout: stale-key semantics, O(1)
//! swap-remove with index fix-up, and ordinal-order iteration (callers'
//! output order depends on it).

use pliron::cols::{ColKey, ColMap, ColSet};


/// Test key: ordinal plus a version, like a `Ptr`'s slot + generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct K(usize, u32);
impl ColKey for K {
    fn ord(&self) -> usize {
        self.0
    }
}

#[test]
fn insert_get_replace_stale() {
    let mut m: ColMap<K, &str> = ColMap::default();
    assert_eq!(m.insert(K(5, 0), "a"), None);
    assert_eq!(m.insert(K(5, 0), "b"), Some("a"));
    assert_eq!(m.get(K(5, 0)), Some(&"b"));
    // Same ordinal, other version: absent, and overwriting it reports none.
    assert_eq!(m.get(K(5, 1)), None);
    assert_eq!(m.insert(K(5, 1), "c"), None);
    assert_eq!(m.get(K(5, 0)), None);
    assert_eq!(m.len(), 1);
    assert_eq!(m.remove(K(5, 0)), None);
    assert_eq!(m.remove(K(5, 1)), Some("c"));
    assert!(m.is_empty());
}

#[test]
fn swap_remove_keeps_index_and_ordinal_iteration() {
    let mut m: ColMap<K, u32> = ColMap::default();
    for o in [9, 2, 7, 0, 4] {
        m.insert(K(o, 0), o as u32 * 10);
    }
    assert_eq!(m.remove(K(9, 0)), Some(90)); // first entry: last one moves into it
    assert_eq!(m.remove(K(7, 0)), Some(70));
    m.insert(K(1, 0), 10);
    for o in [0, 1, 2, 4] {
        assert_eq!(m.get(K(o, 0)), Some(&(o as u32 * 10)));
    }
    assert_eq!(m.get(K(9, 0)), None);
    let ks: Vec<usize> = m.keys().map(|k| k.0).collect();
    assert_eq!(ks, [0, 1, 2, 4]);
    *m.or_default(K(3, 0)) += 3;
    assert_eq!(m.values().copied().collect::<Vec<_>>(), [0, 10, 20, 3, 40]);
    let s: ColSet<K> = [K(3, 0), K(1, 0), K(3, 0)].into_iter().collect();
    assert_eq!(s.len(), 2);
    assert_eq!(s.iter().map(|k| k.0).collect::<Vec<_>>(), [1, 3]);
}
