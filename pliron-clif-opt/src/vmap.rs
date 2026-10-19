//! Dense entity-keyed tables: `FxHashMap<Value, _>` drops in as `VMap`,
//! which replaces hashing with a bounds-checked index into a
//! `Vec<Slot>` and clears in O(1) via a generation stamp. Insertion
//! order is tracked in `keys` so `iter` costs O(present), not O(entity
//! space). Per-slot `(stamp, Option<V>)` makes `remove` a tombstone —
//! a removed key's list entry is filtered on iteration and reinsert
//! needs no list fixup.
//!
//! Use these only for tables whose probe/insert count is on the order
//! of the function's entity space and whose lifetime is a pass or
//! longer — small per-edge scratch maps stay cheaper as hash maps.

use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::{Block, Inst, Value};
use std::marker::PhantomData;

struct Slot<V> {
    stamp: u32,
    val: Option<V>,
}

/// Dense `K → V` map over any `EntityRef` key with hash-map semantics.
pub struct EMap<K: EntityRef, V> {
    slots: Vec<Slot<V>>,
    stamp: u32,
    keys: Vec<K>,
    len: usize,
    _k: PhantomData<K>,
}

/// `Value → V`.
pub type VMap<V> = EMap<Value, V>;
/// `Inst → V`.
pub type IMap<V> = EMap<Inst, V>;
/// `Block → V`.
pub type BMap<V> = EMap<Block, V>;

impl<K: EntityRef, V> Default for EMap<K, V> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            stamp: 1,
            keys: Vec::new(),
            len: 0,
            _k: PhantomData,
        }
    }
}

impl<K: EntityRef, V: Clone> Clone for EMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            slots: self
                .slots
                .iter()
                .map(|s| Slot {
                    stamp: s.stamp,
                    val: s.val.clone(),
                })
                .collect(),
            stamp: self.stamp,
            keys: self.keys.clone(),
            len: self.len,
            _k: PhantomData,
        }
    }
}

impl<K: EntityRef, V> EMap<K, V> {
    /// O(1) reset: bumps the generation so every old slot reads stale.
    /// Keeps `slots`/`keys` capacity for reuse across calls.
    pub fn clear(&mut self) {
        self.keys.clear();
        self.len = 0;
        self.stamp = self.stamp.wrapping_add(1);
        if self.stamp == 0 {
            // 4-billionth clear: rewrite every slot so generation 1 is
            // fresh again rather than colliding with live entries.
            for s in &mut self.slots {
                s.stamp = 0;
                s.val = None;
            }
            self.stamp = 1;
        }
    }

    /// Grow `slots` to cover `k` and ensure its slot is in the current
    /// generation (first touch pushes `keys`). Field-level so callers
    /// can touch `len` while a slot borrow is live.
    #[inline(always)]
    fn touch(&mut self, k: K) -> usize {
        let i = k.index();
        if i >= self.slots.len() {
            self.slots.resize_with(i + 1, || Slot {
                stamp: 0,
                val: None,
            });
        }
        if self.slots[i].stamp != self.stamp {
            self.slots[i].stamp = self.stamp;
            self.slots[i].val = None;
            self.keys.push(k);
        }
        i
    }

    #[inline(always)]
    pub fn get(&self, k: &K) -> Option<&V> {
        self.slots
            .get(k.index())
            .filter(|s| s.stamp == self.stamp)
            .and_then(|s| s.val.as_ref())
    }

    #[inline(always)]
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        let i = k.index();
        if i >= self.slots.len() {
            return None;
        }
        let s = &mut self.slots[i];
        if s.stamp != self.stamp {
            return None;
        }
        s.val.as_mut()
    }

    /// Insert, returning the previous value if the key was present.
    #[inline]
    pub fn insert(&mut self, k: K, x: V) -> Option<V> {
        let i = self.touch(k);
        let old = self.slots[i].val.replace(x);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    /// `entry().or_insert_with` — the slot grows on first touch and a
    /// tombstoned key needs no keys-list fixup.
    #[inline]
    pub fn or_insert_with(&mut self, k: K, f: impl FnOnce() -> V) -> &mut V {
        let i = self.touch(k);
        let s = &mut self.slots[i];
        if s.val.is_none() {
            s.val = Some(f());
            self.len += 1;
        }
        s.val.as_mut().unwrap()
    }

    /// `entry().or_default()`.
    #[inline]
    pub fn or_default(&mut self, k: K) -> &mut V
    where
        V: Default,
    {
        self.or_insert_with(k, V::default)
    }

    /// Remove, returning the value if present. Tombstones the slot; the
    /// keys entry is filtered by `iter`.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let i = k.index();
        if i >= self.slots.len() {
            return None;
        }
        let s = &mut self.slots[i];
        if s.stamp != self.stamp || s.val.is_none() {
            return None;
        }
        self.len -= 1;
        s.val.take()
    }

    #[inline(always)]
    pub fn contains_key(&self, k: &K) -> bool {
        self.get(k).is_some()
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Iterate `(K, &V)` over present entries, insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        let stamp = self.stamp;
        self.keys.iter().filter_map(move |&k| {
            let s = &self.slots[k.index()];
            (s.stamp == stamp)
                .then(|| s.val.as_ref().map(|v| (k, v)))
                .flatten()
        })
    }

    /// Iterate present keys, insertion order.
    pub fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.iter().map(|(k, _)| k)
    }

    /// Iterate present values, insertion order.
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }
}

impl<'a, K: EntityRef, V> IntoIterator for &'a EMap<K, V> {
    type Item = (K, &'a V);
    type IntoIter = Box<dyn Iterator<Item = (K, &'a V)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<K: EntityRef + std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for EMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: EntityRef, V> std::iter::FromIterator<(K, V)> for EMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(it: T) -> Self {
        let mut m = Self::default();
        for (k, v) in it {
            m.insert(k, v);
        }
        m
    }
}

impl<K: EntityRef, V> IntoIterator for EMap<K, V> {
    type Item = (K, V);
    type IntoIter = std::vec::IntoIter<(K, V)>;
    fn into_iter(mut self) -> Self::IntoIter {
        let stamp = self.stamp;
        let slots = &mut self.slots;
        let out: Vec<(K, V)> = self
            .keys
            .iter()
            .filter_map(|&k| {
                let s = &mut slots[k.index()];
                (s.stamp == stamp).then(|| s.val.take().map(|v| (k, v))).flatten()
            })
            .collect();
        out.into_iter()
    }
}

/// Dense `K` set — membership by generation stamp, O(1) clear.
pub struct ESet<K: EntityRef> {
    slots: Vec<u32>,
    stamp: u32,
    keys: Vec<K>,
    len: usize,
}

/// `Value` set.
pub type VSet = ESet<Value>;
/// `Inst` set.
pub type ISet = ESet<Inst>;
/// `Block` set.
pub type BSet = ESet<Block>;

impl<K: EntityRef> Default for ESet<K> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            stamp: 1,
            keys: Vec::new(),
            len: 0,
        }
    }
}

impl<K: EntityRef> Clone for ESet<K> {
    fn clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            stamp: self.stamp,
            keys: self.keys.clone(),
            len: self.len,
        }
    }
}

impl<K: EntityRef> ESet<K> {
    pub fn clear(&mut self) {
        self.keys.clear();
        self.len = 0;
        self.stamp = self.stamp.wrapping_add(1);
        if self.stamp == 0 {
            self.slots.iter_mut().for_each(|s| *s = 0);
            self.stamp = 1;
        }
    }

    #[inline(always)]
    pub fn contains(&self, k: &K) -> bool {
        self.slots.get(k.index()).is_some_and(|&s| s == self.stamp)
    }

    /// Insert; returns true if the key was absent.
    #[inline]
    pub fn insert(&mut self, k: K) -> bool {
        let i = k.index();
        if i >= self.slots.len() {
            self.slots.resize(i + 1, 0);
        }
        if self.slots[i] == self.stamp {
            return false;
        }
        self.slots[i] = self.stamp;
        self.keys.push(k);
        self.len += 1;
        true
    }

    pub fn remove(&mut self, k: &K) -> bool {
        let i = k.index();
        if i >= self.slots.len() || self.slots[i] != self.stamp {
            return false;
        }
        self.slots[i] = self.stamp.wrapping_sub(1);
        self.len -= 1;
        true
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Present members, insertion order (skips removed entries).
    pub fn iter(&self) -> impl Iterator<Item = K> + '_ {
        let stamp = self.stamp;
        self.keys
            .iter()
            .copied()
            .filter(move |&k| self.slots[k.index()] == stamp)
    }
}

impl<K: EntityRef> std::iter::FromIterator<K> for ESet<K> {
    fn from_iter<T: IntoIterator<Item = K>>(it: T) -> Self {
        let mut s = Self::default();
        for k in it {
            s.insert(k);
        }
        s
    }
}
