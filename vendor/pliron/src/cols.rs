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
use core::marker::PhantomData;

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
/// lookup is a bounds check + key compare, no hashing.
#[derive(Clone, Debug)]
pub struct ColMap<K: ColKey, V> {
    slots: Vec<Option<(K, V)>>,
    live: usize,
    _k: PhantomData<K>,
}

impl<K: ColKey, V> Default for ColMap<K, V> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            live: 0,
            _k: PhantomData,
        }
    }
}

impl<K: ColKey, V> ColMap<K, V> {
    #[inline]
    pub fn get(&self, k: K) -> Option<&V> {
        self.slots
            .get(k.ord())
            .and_then(Option::as_ref)
            .filter(|(ek, _)| *ek == k)
            .map(|(_, v)| v)
    }
    #[inline]
    pub fn get_mut(&mut self, k: K) -> Option<&mut V> {
        self.slots
            .get_mut(k.ord())
            .and_then(Option::as_mut)
            .filter(|(ek, _)| *ek == k)
            .map(|(_, v)| v)
    }
    /// `HashMap::insert` shape: returns the previous value for the same
    /// key; a stale-keyed slot is overwritten without one.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let o = k.ord();
        if o >= self.slots.len() {
            self.slots.resize_with(o + 1, || None);
        }
        match self.slots[o].replace((k, v)) {
            None => {
                self.live += 1;
                None
            }
            Some((ek, ev)) if ek == k => Some(ev),
            Some(_) => None,
        }
    }
    pub fn remove(&mut self, k: K) -> Option<V> {
        let o = k.ord();
        let slot = self.slots.get_mut(o)?;
        if matches!(slot, Some((ek, _)) if *ek == k) {
            self.live -= 1;
            slot.take().map(|(_, v)| v)
        } else {
            None
        }
    }
    #[inline]
    pub fn contains(&self, k: K) -> bool {
        self.get(k).is_some()
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.live
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        self.slots.iter().flatten().map(|(k, v)| (*k, v))
    }
    pub fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.slots.iter().flatten().map(|(k, _)| *k)
    }
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.slots.iter().flatten().map(|(_, v)| v)
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
