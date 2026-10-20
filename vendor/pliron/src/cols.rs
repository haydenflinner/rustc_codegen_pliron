//! PLIRON: dense columnar side tables — `Vec`-indexed maps/sets keyed by
//! an entity's dense ordinal ([Ptr] arena slot index or [Value] uid),
//! replacing `FxHashMap<K, V>` where probes dominate writes.
//!
//! Slots hold the key beside the value, so `iter`/`keys`/`values` recover
//! entries and a slot reused by a different entity (stale key) compares
//! unequal instead of colliding — the same dangling-key semantics a hash
//! map has when entities are deleted but their facts stay.

use crate::basic_block::BasicBlock;
use crate::context::Ptr;
use crate::operation::Operation;
use crate::value::Value;
use alloc::vec::Vec;

/// Key into a [ColMap]/[ColSet] — a dense per-[Context](crate::context::Context)
/// ordinal. `Ptr` uses its arena slot index (version excluded — stored
/// keys carry it, so stale slots compare unequal); `Value` uses its
/// monotonic uid.
pub trait ColKey: Copy + Eq {
    /// The dense ordinal this key maps to.
    fn ord(&self) -> usize;
}
impl ColKey for Value {
    #[inline]
    fn ord(&self) -> usize {
        self.uid() as usize
    }
}
impl ColKey for Ptr<Operation> {
    #[inline]
    fn ord(&self) -> usize {
        self.ord()
    }
}
impl ColKey for Ptr<BasicBlock> {
    #[inline]
    fn ord(&self) -> usize {
        self.ord()
    }
}

/// Dense column replacing `FxHashMap<K, V>` where probes dominate:
/// lookup is a bounds check + an index load + key compare, no hashing.
///
/// Sparse-set layout: `idx` spans the whole ordinal space at 4 bytes per
/// entity, `ents` holds only the live entries. Inline `Option<(K, V)>`
/// slots cost `size_of::<(K, V)>()` per *entity in the Context* — a
/// `CallInfo` column reserved ~150 B for every op although few are calls,
/// and every growth step memmoved the lot (inlining mints fresh ordinals
/// constantly).
#[derive(Clone, Debug)]
pub struct ColMap<K: ColKey, V> {
    /// Ordinal → 1 + position in `ents`; 0 = empty. Every entry `e` is
    /// referenced from exactly `idx[e.0.ord()]`.
    idx: Vec<u32>,
    ents: Vec<(K, V)>,
}

impl<K: ColKey, V> Default for ColMap<K, V> {
    fn default() -> Self {
        Self {
            idx: Vec::new(),
            ents: Vec::new(),
        }
    }
}

impl<K: ColKey, V> ColMap<K, V> {
    /// The `ents` position for ordinal `o`, if occupied (by any key).
    #[inline]
    fn slot(&self, o: usize) -> Option<usize> {
        match self.idx.get(o) {
            Some(&i) if i != 0 => Some(i as usize - 1),
            _ => None,
        }
    }
    #[inline]
    pub fn get(&self, k: K) -> Option<&V> {
        let e = &self.ents[self.slot(k.ord())?];
        (e.0 == k).then_some(&e.1)
    }
    #[inline]
    pub fn get_mut(&mut self, k: K) -> Option<&mut V> {
        let i = self.slot(k.ord())?;
        let e = &mut self.ents[i];
        (e.0 == k).then_some(&mut e.1)
    }
    /// `HashMap::insert` shape: returns the previous value for the same
    /// key; a stale-keyed slot is overwritten without one.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let o = k.ord();
        if let Some(i) = self.slot(o) {
            let (ek, ev) = core::mem::replace(&mut self.ents[i], (k, v));
            return (ek == k).then_some(ev);
        }
        if o >= self.idx.len() {
            self.idx.resize(o + 1, 0);
        }
        self.ents.push((k, v));
        self.idx[o] = u32::try_from(self.ents.len()).expect("ColMap: > u32::MAX entries");
        None
    }
    pub fn remove(&mut self, k: K) -> Option<V> {
        let o = k.ord();
        let i = self.slot(o)?;
        if self.ents[i].0 != k {
            return None;
        }
        self.idx[o] = 0;
        let (_, v) = self.ents.swap_remove(i);
        if let Some(moved) = self.ents.get(i) {
            self.idx[moved.0.ord()] = i as u32 + 1;
        }
        Some(v)
    }
    #[inline]
    pub fn contains(&self, k: K) -> bool {
        self.get(k).is_some()
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.ents.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ents.is_empty()
    }
    /// Entries in ordinal order (walks the index, as the inline-slot
    /// layout did — callers' output order depends on it).
    fn ordered(&self) -> impl Iterator<Item = &(K, V)> {
        self.idx
            .iter()
            .filter(|&&i| i != 0)
            .map(|&i| &self.ents[i as usize - 1])
    }
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        self.ordered().map(|(k, v)| (*k, v))
    }
    pub fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.ordered().map(|(k, _)| *k)
    }
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.ordered().map(|(_, v)| v)
    }
    /// `entry(k).or_insert_with(f)` shape — returns the existing or
    /// freshly inserted slot's value.
    pub fn or_insert_with(&mut self, k: K, f: impl FnOnce() -> V) -> &mut V {
        if !self.contains(k) {
            self.insert(k, f());
        }
        self.get_mut(k).expect("or_insert_with: key vanished")
    }
    /// `entry(k).or_default()` shape — requires `V: Default`.
    pub fn or_default(&mut self, k: K) -> &mut V
    where
        V: Default,
    {
        self.or_insert_with(k, V::default)
    }
}

impl<K: ColKey, V> core::ops::Index<K> for ColMap<K, V> {
    type Output = V;
    #[inline]
    fn index(&self, k: K) -> &V {
        self.get(k).expect("ColMap index of absent key")
    }
}

impl<K: ColKey, V> FromIterator<(K, V)> for ColMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(it: I) -> Self {
        let mut m = Self::default();
        for (k, v) in it {
            m.insert(k, v);
        }
        m
    }
}

impl<K: ColKey> FromIterator<K> for ColSet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(it: I) -> Self {
        let mut s = Self::default();
        for k in it {
            s.insert(k);
        }
        s
    }
}

/// Dense set — [ColMap] with a `()` value and set-style verbs.
#[derive(Clone, Debug)]
pub struct ColSet<K: ColKey>(pub ColMap<K, ()>);

impl<K: ColKey> Default for ColSet<K> {
    fn default() -> Self {
        Self(ColMap::default())
    }
}

impl<K: ColKey> ColSet<K> {
    #[inline]
    pub fn contains(&self, k: K) -> bool {
        self.0.contains(k)
    }
    /// Returns true if the key was newly inserted (HashSet::insert shape).
    pub fn insert(&mut self, k: K) -> bool {
        self.0.insert(k, ()).is_none()
    }
    pub fn remove(&mut self, k: K) -> bool {
        self.0.remove(k).is_some()
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = K> + '_ {
        self.0.keys()
    }
}

