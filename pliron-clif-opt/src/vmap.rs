//! Dense `Value`-keyed tables: `FxHashMap<Value, _>` drops in as `VMap`,
//! which replaces hashing with a bounds-checked index into a
//! `Vec<Slot>` and clears in O(1) via a generation stamp. Insertion
//! order is tracked in `keys` so `iter` costs O(present), not O(value
//! space). Per-slot `(stamp, Option<V>)` makes `remove` a tombstone —
//! a removed key's list entry is filtered on iteration and reinsert
//! needs no list fixup.
//!
//! Use these only for tables whose probe/insert count is on the order
//! of the function's value space and whose lifetime is a pass or
//! longer — small per-edge scratch maps stay cheaper as hash maps.

use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::Value;

struct Slot<V> {
    stamp: u32,
    val: Option<V>,
}

/// Dense `Value → V` map with hash-map semantics.
pub struct VMap<V> {
    slots: Vec<Slot<V>>,
    stamp: u32,
    keys: Vec<Value>,
    len: usize,
}

impl<V> Default for VMap<V> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            stamp: 1,
            keys: Vec::new(),
            len: 0,
        }
    }
}

impl<V> Clone for VMap<V>
where
    V: Clone,
{
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
        }
    }
}

impl<V> VMap<V> {
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

    /// Grow `slots` to cover `v` and ensure its slot is in the current
    /// generation (first touch pushes `keys`). Field-level so callers
    /// can touch `len` while a slot borrow is live.
    #[inline(always)]
    fn touch(&mut self, v: Value) -> usize {
        let i = v.index();
        if i >= self.slots.len() {
            self.slots.resize_with(i + 1, || Slot {
                stamp: 0,
                val: None,
            });
        }
        if self.slots[i].stamp != self.stamp {
            self.slots[i].stamp = self.stamp;
            self.slots[i].val = None;
            self.keys.push(v);
        }
        i
    }

    #[inline(always)]
    pub fn get(&self, v: &Value) -> Option<&V> {
        self.slots
            .get(v.index())
            .filter(|s| s.stamp == self.stamp)
            .and_then(|s| s.val.as_ref())
    }

    #[inline(always)]
    pub fn get_mut(&mut self, v: &Value) -> Option<&mut V> {
        let i = v.index();
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
    pub fn insert(&mut self, v: Value, x: V) -> Option<V> {
        let i = self.touch(v);
        let old = self.slots[i].val.replace(x);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    /// `entry().or_insert_with` — the slot grows on first touch and a
    /// tombstoned key needs no keys-list fixup.
    #[inline]
    pub fn or_insert_with(&mut self, v: Value, f: impl FnOnce() -> V) -> &mut V {
        let i = self.touch(v);
        let s = &mut self.slots[i];
        if s.val.is_none() {
            s.val = Some(f());
            self.len += 1;
        }
        s.val.as_mut().unwrap()
    }

    /// `entry().or_default()`.
    #[inline]
    pub fn or_default(&mut self, v: Value) -> &mut V
    where
        V: Default,
    {
        self.or_insert_with(v, V::default)
    }

    /// Remove, returning the value if present. Tombstones the slot; the
    /// keys entry is filtered by `iter`.
    pub fn remove(&mut self, v: &Value) -> Option<V> {
        let i = v.index();
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
    pub fn contains_key(&self, v: &Value) -> bool {
        self.get(v).is_some()
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Iterate `(Value, &V)` over present entries, insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (Value, &V)> {
        let stamp = self.stamp;
        self.keys.iter().filter_map(move |&k| {
            let s = &self.slots[k.index()];
            (s.stamp == stamp)
                .then(|| s.val.as_ref().map(|v| (k, v)))
                .flatten()
        })
    }

    /// Iterate present keys, insertion order.
    pub fn keys(&self) -> impl Iterator<Item = Value> + '_ {
        self.iter().map(|(k, _)| k)
    }

    /// Iterate present values, insertion order.
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }
}

impl<'a, V> IntoIterator for &'a VMap<V> {
    type Item = (Value, &'a V);
    type IntoIter = Box<dyn Iterator<Item = (Value, &'a V)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<V: std::fmt::Debug> std::fmt::Debug for VMap<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<V> std::iter::FromIterator<(Value, V)> for VMap<V> {
    fn from_iter<T: IntoIterator<Item = (Value, V)>>(it: T) -> Self {
        let mut m = Self::default();
        for (k, v) in it {
            m.insert(k, v);
        }
        m
    }
}

impl<V> IntoIterator for VMap<V> {
    type Item = (Value, V);
    type IntoIter = std::vec::IntoIter<(Value, V)>;
    fn into_iter(mut self) -> Self::IntoIter {
        let stamp = self.stamp;
        let slots = &mut self.slots;
        let out: Vec<(Value, V)> = self
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

/// Dense `Value` set — membership by generation stamp, O(1) clear.
pub struct VSet {
    slots: Vec<u32>,
    stamp: u32,
    keys: Vec<Value>,
    len: usize,
}

impl Default for VSet {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            stamp: 1,
            keys: Vec::new(),
            len: 0,
        }
    }
}

impl Clone for VSet {
    fn clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            stamp: self.stamp,
            keys: self.keys.clone(),
            len: self.len,
        }
    }
}

impl VSet {
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
    pub fn contains(&self, v: &Value) -> bool {
        self.slots
            .get(v.index())
            .is_some_and(|&s| s == self.stamp)
    }

    /// Insert; returns true if the value was absent.
    #[inline]
    pub fn insert(&mut self, v: Value) -> bool {
        let i = v.index();
        if i >= self.slots.len() {
            self.slots.resize(i + 1, 0);
        }
        if self.slots[i] == self.stamp {
            return false;
        }
        self.slots[i] = self.stamp;
        self.keys.push(v);
        self.len += 1;
        true
    }

    pub fn remove(&mut self, v: &Value) -> bool {
        let i = v.index();
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
    pub fn iter(&self) -> impl Iterator<Item = Value> + '_ {
        let stamp = self.stamp;
        self.keys
            .iter()
            .copied()
            .filter(move |&k| self.slots[k.index()] == stamp)
    }
}

impl std::iter::FromIterator<Value> for VSet {
    fn from_iter<T: IntoIterator<Item = Value>>(it: T) -> Self {
        let mut s = Self::default();
        for v in it {
            s.insert(v);
        }
        s
    }
}
