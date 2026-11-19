//! An insertion-ordered hash map of values, the storage of `Map` and `Set` (DESIGN §5.2).
//!
//! Entries live in a vector in insertion order; a hash index maps the hash of a key to the
//! positions of the entries with that hash. Removing an entry leaves a tombstone so the other
//! positions stay valid (an iteration holds positions while the mutation guard blocks any
//! removal); the vector is compacted when tombstones outnumber live entries. Lookup, insert and
//! remove are O(1) on average. Keys are compared with the structural equality of
//! [`values_equal`], so a key is stored once, in the entry, and the index holds no second
//! reference to it.
//!
//! This module is pure data structure code: it never runs Colibri code.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::compare::values_equal;
use crate::value::Value;

/// Hashing gives up (and returns a constant) below this nesting, which only costs speed.
const MAX_HASH_DEPTH: u32 = 64;

/// Compacts when more than this many tombstones exist and they outnumber live entries.
const COMPACT_MIN: usize = 32;

/// The hash of a key value. Equal keys hash equally: strings by content, aggregates by their
/// parts, class objects by identity. (The checker only lets hashable types be keys; anything
/// else still hashes without panicking.)
pub fn hash_value(v: &Value) -> u64 {
    let mut h = DefaultHasher::new();
    feed(v, &mut h, 0);
    h.finish()
}

fn feed(v: &Value, h: &mut DefaultHasher, depth: u32) {
    if depth > MAX_HASH_DEPTH {
        return;
    }
    std::mem::discriminant(v).hash(h);
    match v {
        Value::Unit | Value::Weak(_) | Value::Closure(_) | Value::Cell(_) | Value::Iter(_) => {}
        Value::Fn(f, _) => f.0.hash(h),
        Value::Range(r) => {
            r.start.hash(h);
            r.end.hash(h);
            r.inclusive.hash(h);
        }
        Value::Bool(b) => b.hash(h),
        Value::Int(i) => i.hash(h),
        Value::U64(i) => i.hash(h),
        Value::Ptr(p) => p.hash(h),
        Value::Float(x) => x.to_bits().hash(h),
        Value::F32(x) => x.to_bits().hash(h),
        Value::Vec(v) => v.c[..v.n as usize].iter().for_each(|x| x.to_bits().hash(h)),
        Value::IVec(v) => v.hash(h),
        Value::Char(c) => c.hash(h),
        Value::Str(s) => s.hash(h),
        Value::Tuple(items) => items.iter().for_each(|x| feed(x, h, depth + 1)),
        Value::Struct(s) => {
            s.ty.hash(h);
            s.fields.iter().for_each(|x| feed(x, h, depth + 1));
        }
        Value::Enum(e) => {
            e.ty.hash(h);
            e.variant.hash(h);
            e.payload.iter().for_each(|x| feed(x, h, depth + 1));
        }
        Value::Obj(rc) => (Rc::as_ptr(rc) as usize).hash(h),
        Value::Host(r) => (r.index, r.generation).hash(h),
        Value::Dyn(d) => feed(&d.val, h, depth + 1),
        Value::List(l) => l.len().hash(h),
        Value::Map(m) => m.len().hash(h),
        Value::Set(s) => s.len().hash(h),
    }
}

/// The entry positions with one hash.
#[derive(Debug)]
enum Bucket {
    One(usize),
    Many(Vec<usize>),
}

impl Bucket {
    fn positions(&self) -> &[usize] {
        match self {
            Bucket::One(p) => std::slice::from_ref(p),
            Bucket::Many(v) => v,
        }
    }
}

/// An insertion-ordered map from values to values.
#[derive(Debug, Default)]
pub struct OrderedMap {
    entries: Vec<Option<(Value, Value)>>,
    index: HashMap<u64, Bucket>,
    live: usize,
}

impl OrderedMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entries are present.
    pub fn len(&self) -> usize {
        self.live
    }

    /// How many positions exist, live or removed: the bound of [`OrderedMap::entry_at`].
    pub fn slots(&self) -> usize {
        self.entries.len()
    }

    /// The entry at a position, if it is live.
    pub fn entry_at(&self, slot: usize) -> Option<&(Value, Value)> {
        self.entries.get(slot).and_then(Option::as_ref)
    }

    fn find(&self, hash: u64, key: &Value) -> Option<usize> {
        let bucket = self.index.get(&hash)?;
        bucket.positions().iter().copied().find(|&p| {
            self.entries[p]
                .as_ref()
                .is_some_and(|(k, _)| values_equal(k, key).unwrap_or(false))
        })
    }

    pub fn get(&self, key: &Value) -> Option<&Value> {
        let p = self.find(hash_value(key), key)?;
        self.entries[p].as_ref().map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &Value) -> Option<&mut Value> {
        let p = self.find(hash_value(key), key)?;
        self.entries[p].as_mut().map(|(_, v)| v)
    }

    pub fn contains(&self, key: &Value) -> bool {
        self.find(hash_value(key), key).is_some()
    }

    /// Adds or replaces an entry. A replaced entry keeps its position and its original key.
    /// Returns the previous value and, in that case, the new key that was not needed; the
    /// caller releases both.
    pub fn insert(&mut self, key: Value, value: Value) -> (Option<Value>, Option<Value>) {
        let hash = hash_value(&key);
        if let Some(p) = self.find(hash, &key)
            && let Some((_, slot)) = self.entries[p].as_mut()
        {
            let old = std::mem::replace(slot, value);
            return (Some(old), Some(key));
        }
        let pos = self.entries.len();
        self.entries.push(Some((key, value)));
        self.live += 1;
        match self.index.get_mut(&hash) {
            None => {
                self.index.insert(hash, Bucket::One(pos));
            }
            Some(bucket) => match bucket {
                Bucket::One(first) => *bucket = Bucket::Many(vec![*first, pos]),
                Bucket::Many(v) => v.push(pos),
            },
        }
        (None, None)
    }

    /// Removes an entry, returning its key and value.
    pub fn remove(&mut self, key: &Value) -> Option<(Value, Value)> {
        let hash = hash_value(key);
        let p = self.find(hash, key)?;
        let removed = self.entries[p].take();
        self.live -= 1;
        let emptied = match self.index.get_mut(&hash) {
            Some(Bucket::One(_)) | None => true,
            Some(Bucket::Many(v)) => {
                v.retain(|&q| q != p);
                v.is_empty()
            }
        };
        if emptied {
            self.index.remove(&hash);
        }
        if self.entries.len() - self.live > COMPACT_MIN && self.entries.len() > 2 * self.live {
            self.compact();
        }
        removed
    }

    /// Drops the tombstones and renumbers the index.
    fn compact(&mut self) {
        let old = std::mem::take(&mut self.entries);
        self.index.clear();
        for (k, v) in old.into_iter().flatten() {
            let hash = hash_value(&k);
            let pos = self.entries.len();
            self.entries.push(Some((k, v)));
            match self.index.get_mut(&hash) {
                None => {
                    self.index.insert(hash, Bucket::One(pos));
                }
                Some(bucket) => match bucket {
                    Bucket::One(first) => *bucket = Bucket::Many(vec![*first, pos]),
                    Bucket::Many(v) => v.push(pos),
                },
            }
        }
    }

    /// Live entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &(Value, Value)> {
        self.entries.iter().flatten()
    }

    /// Removes and returns every entry in insertion order.
    pub fn take_all(&mut self) -> Vec<(Value, Value)> {
        self.index.clear();
        self.live = 0;
        std::mem::take(&mut self.entries)
            .into_iter()
            .flatten()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(i: i64) -> Value {
        Value::Int(i)
    }

    #[test]
    fn keeps_insertion_order_and_replaces_in_place() {
        let mut m = OrderedMap::new();
        for k in [3, 1, 2] {
            assert_eq!(m.insert(int(k), int(k * 10)), (None, None));
        }
        assert_eq!(m.insert(int(1), int(99)), (Some(int(10)), Some(int(1))));
        let keys: Vec<i64> = m
            .iter()
            .map(|(k, _)| if let Value::Int(i) = k { *i } else { -1 })
            .collect();
        assert_eq!(keys, [3, 1, 2]);
        assert_eq!(m.get(&int(1)), Some(&int(99)));
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn removal_leaves_order_and_survives_compaction() {
        let mut m = OrderedMap::new();
        for k in 0..200 {
            m.insert(int(k), int(k));
        }
        for k in 0..150 {
            assert_eq!(m.remove(&int(k)), Some((int(k), int(k))));
        }
        assert_eq!(m.len(), 50);
        assert_eq!(m.remove(&int(0)), None);
        let first = m.iter().next().map(|(k, _)| k.clone());
        assert_eq!(first, Some(int(150)));
        assert_eq!(m.get(&int(199)), Some(&int(199)));
        m.insert(int(5), int(5));
        assert_eq!(m.iter().last().map(|(k, _)| k.clone()), Some(int(5)));
    }

    #[test]
    fn composite_keys_compare_structurally() {
        let mut m = OrderedMap::new();
        let key = |a: i64, b: &str| Value::tuple(vec![int(a), Value::str(b)]);
        m.insert(key(1, "a"), int(1));
        m.insert(key(1, "b"), int(2));
        assert_eq!(m.get(&key(1, "b")), Some(&int(2)));
        assert_eq!(m.get(&key(2, "a")), None);
        assert_eq!(m.insert(key(1, "a"), int(3)).0, Some(int(1)));
        assert_eq!(m.len(), 2);
    }
}
