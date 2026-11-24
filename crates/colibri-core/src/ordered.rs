//! An insertion-ordered hash map of values, the storage of `Map` and `Set` (DESIGN §5.2).
//!
//! Entries live in a vector in insertion order, each with the hash of its key; an open-addressing
//! index (linear probing over `u32` positions, like CPython's dict) finds the position of the
//! entry for a hash. Removing an entry leaves a tombstone so the other
//! positions stay valid (an iteration holds positions while the mutation guard blocks any
//! removal); the vector is compacted when tombstones outnumber live entries. Lookup, insert and
//! remove are O(1) on average. Keys are compared with the structural equality of
//! `crate::compare::values_equal`, so a key is stored once, in the entry, and the index holds
//! no second reference to it.
//!
//! This module is pure data structure code: it never runs Colibri code.

use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::compare::equal_or_give_up;
use crate::value::Value;

/// Hashing gives up (and returns a constant) below this nesting, which only costs speed.
const MAX_HASH_DEPTH: u32 = 64;

/// Compacts when more than this many tombstones exist and they outnumber live entries.
const COMPACT_MIN: usize = 32;

/// The hash of a key value. Equal keys hash equally: strings by content, aggregates by their
/// parts, class objects by identity. (The checker only lets hashable types be keys; anything
/// else still hashes without panicking.)
#[inline]
pub(crate) fn hash_value(v: &Value) -> u64 {
    // The commonest keys, without the walk over a value that aggregates need (same hash).
    match v {
        Value::Int(i) => {
            let mut h = KeyHasher::default();
            std::mem::discriminant(v).hash(&mut h);
            i.hash(&mut h);
            h.finish()
        }
        Value::Str(s) => {
            let mut h = KeyHasher::default();
            std::mem::discriminant(v).hash(&mut h);
            s.hash(&mut h);
            h.finish()
        }
        _ => hash_value_with(v, &mut |_| None),
    }
}

/// [`hash_value`], where `host` may give the hash of an object of a host class (at any depth)
/// in place of its handle. Used for the `hash()` that scripts see: an object that a host class
/// compares by its own `==` must hash by what that `==` looks at.
pub(crate) fn hash_value_with(v: &Value, host: &mut dyn FnMut(&Value) -> Option<u64>) -> u64 {
    let mut h = KeyHasher::default();
    feed(v, &mut h, 0, host);
    h.finish()
}

/// The hasher of keys. Keys are small (integers, short strings, tuples of them), so SipHash's
/// cost buys little. Each word is folded in as the two halves of a 128-bit product, xored:
/// `x = (state ^ word) * odd constant; state = lo(x) ^ hi(x)` (one multiply instruction). The
/// plain FxHash step (`rotl(state, 5) ^ word`, times a constant) only carries differences
/// upward, so a change in the top byte of one 8-byte chunk could be cancelled by the next
/// chunk's low byte, and it collided on ordinary strings that cross an 8-byte boundary; a
/// 64-bit multiply followed by `x ^ (x >> 32)` still left a few pairs of 15-byte strings
/// equal. The index below uses the hash as it is, so [`KeyHasher::finish`] runs a
/// full-avalanche finalizer (the MurmurHash3 64-bit mix) to spread the low bits that pick an
/// index slot. The hash is not a secret: `x.hash()` returns it
/// to scripts, and nothing here defends against keys chosen to collide (a collision only costs
/// speed, because buckets compare keys with [`values_equal`]). The tests below count full
/// 64-bit collisions and slot loads for several families of keys.
#[derive(Default)]
struct KeyHasher {
    state: u64,
}

/// The FxHash multiplier (an odd constant with well-spread bits).
const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl KeyHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        let x = u128::from(self.state ^ word) * u128::from(FX_SEED);
        self.state = (x as u64) ^ ((x >> 64) as u64);
    }
}

impl Hasher for KeyHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (words, tail) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        if !tail.is_empty() {
            let mut word = 0u64;
            for (i, b) in tail.iter().enumerate() {
                word |= u64::from(*b) << (8 * i);
            }
            // The length goes in the top byte (a tail has at most 7 bytes), so "a" and "a\0"
            // differ.
            self.add(word | ((tail.len() as u64) << 56));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn write_i8(&mut self, i: i8) {
        self.add(i as u64);
    }

    #[inline]
    fn write_i16(&mut self, i: i16) {
        self.add(i as u64);
    }

    #[inline]
    fn write_i32(&mut self, i: i32) {
        self.add(i as u64);
    }

    #[inline]
    fn write_i64(&mut self, i: i64) {
        self.add(i as u64);
    }

    #[inline]
    fn write_isize(&mut self, i: isize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        // MurmurHash3 fmix64.
        let mut x = self.state;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 33;
        x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        x ^ (x >> 33)
    }
}

fn feed(v: &Value, h: &mut KeyHasher, depth: u32, host: &mut dyn FnMut(&Value) -> Option<u64>) {
    if depth > MAX_HASH_DEPTH {
        return;
    }
    std::mem::discriminant(v).hash(h);
    match v {
        Value::Unit
        | Value::Weak(_)
        | Value::Closure(_)
        | Value::Cell(_)
        | Value::ByteBuf(_)
        | Value::Iter(_) => {}
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
        Value::Char(c) => c.hash(h),
        Value::Str(s) => s.hash(h),
        Value::Bytes(b) => b.hash(h),
        Value::Tuple(items) => items.iter().for_each(|x| feed(x, h, depth + 1, host)),
        Value::Struct(s) => {
            s.ty.hash(h);
            s.fields.iter().for_each(|x| feed(x, h, depth + 1, host));
        }
        Value::Enum(e) => {
            e.ty.hash(h);
            e.variant.hash(h);
            e.payload.iter().for_each(|x| feed(x, h, depth + 1, host));
        }
        Value::Obj(rc) => (Rc::as_ptr(rc) as usize).hash(h),
        Value::Host(r) => match host(v) {
            Some(own) => own.hash(h),
            None => (r.index, r.generation).hash(h),
        },
        Value::Dyn(d) => feed(&d.val, h, depth + 1, host),
        Value::List(l) => l.len().hash(h),
        Value::Map(m) => m.len().hash(h),
        Value::Set(s) => s.len().hash(h),
    }
}

/// An index slot that has never held a position.
const EMPTY: u32 = u32::MAX;
/// An index slot whose entry was removed; probing continues past it.
const DUMMY: u32 = u32::MAX - 1;

/// One position of the entry vector: the key's hash, and the pair unless it was removed.
#[derive(Debug)]
struct Entry {
    hash: u64,
    kv: Option<(Value, Value)>,
}

/// An insertion-ordered map from values to values.
///
/// `index` is empty or a power-of-two number of slots, each [`EMPTY`], [`DUMMY`] or a position
/// in `entries`. Every position of `entries` (live or removed) owns one slot that is not
/// `EMPTY` until the index is rebuilt, and `used` counts those, so the index is never more than
/// two thirds full and a probe always ends at an `EMPTY` slot.
#[derive(Debug, Default)]
pub struct OrderedMap {
    entries: Vec<Entry>,
    index: Vec<u32>,
    /// Slots that are not `EMPTY`: live entries plus `DUMMY`s.
    used: usize,
    live: usize,
}

impl OrderedMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entries are present.
    #[inline]
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether no entry is present.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// How many positions exist, live or removed: the bound of `OrderedMap::entry_at`.
    #[inline]
    pub fn slots(&self) -> usize {
        self.entries.len()
    }

    /// The entry at a position, if it is live.
    #[inline]
    pub(crate) fn entry_at(&self, slot: usize) -> Option<&(Value, Value)> {
        self.entries.get(slot).and_then(|e| e.kv.as_ref())
    }

    /// The position of the entry for `key`, or the index slot where it would be inserted.
    #[inline]
    fn probe(&self, hash: u64, key: &Value) -> Result<usize, usize> {
        let mask = self.index.len().wrapping_sub(1);
        if self.index.is_empty() {
            return Err(0);
        }
        let mut i = hash as usize & mask;
        let mut free = usize::MAX;
        loop {
            match self.index[i] {
                EMPTY => return Err(if free == usize::MAX { i } else { free }),
                DUMMY => {
                    if free == usize::MAX {
                        free = i;
                    }
                }
                p => {
                    let e = &self.entries[p as usize];
                    if e.hash == hash
                        && let Some((k, _)) = &e.kv
                        && equal_or_give_up(k, key)
                    {
                        return Ok(p as usize);
                    }
                }
            }
            i = (i + 1) & mask;
        }
    }

    #[inline]
    fn find(&self, hash: u64, key: &Value) -> Option<usize> {
        self.probe(hash, key).ok()
    }

    #[inline]
    pub fn get(&self, key: &Value) -> Option<&Value> {
        let p = self.find(hash_value(key), key)?;
        self.entries[p].kv.as_ref().map(|(_, v)| v)
    }

    #[inline]
    pub fn get_mut(&mut self, key: &Value) -> Option<&mut Value> {
        let p = self.find(hash_value(key), key)?;
        self.entries[p].kv.as_mut().map(|(_, v)| v)
    }

    #[inline]
    pub fn contains(&self, key: &Value) -> bool {
        self.find(hash_value(key), key).is_some()
    }

    /// Adds or replaces an entry. A replaced entry keeps its position and its original key.
    /// Returns the previous value and, in that case, the new key that was not needed; the
    /// caller releases both.
    #[inline]
    pub fn insert(&mut self, key: Value, value: Value) -> (Option<Value>, Option<Value>) {
        let hash = hash_value(&key);
        self.insert_hashed(hash, key, value)
    }

    /// [`OrderedMap::insert`] with the hash of `key` already computed (tests pass chosen
    /// hashes to force collisions).
    fn insert_hashed(
        &mut self,
        hash: u64,
        key: Value,
        value: Value,
    ) -> (Option<Value>, Option<Value>) {
        let mut slot = match self.probe(hash, &key) {
            Ok(p) => {
                if let Some((_, old)) = self.entries[p].kv.as_mut() {
                    return (Some(std::mem::replace(old, value)), Some(key));
                }
                unreachable!("the index names a removed entry");
            }
            Err(slot) => slot,
        };
        if (self.used + 1) * 3 > self.index.len() * 2 {
            self.rebuild_index();
            slot = self.free_slot(hash);
        }
        let pos = self.entries.len();
        assert!(pos < DUMMY as usize, "map too large");
        if self.index.get(slot) == Some(&EMPTY) {
            self.used += 1;
        }
        self.index[slot] = pos as u32;
        self.entries.push(Entry {
            hash,
            kv: Some((key, value)),
        });
        self.live += 1;
        (None, None)
    }

    /// The first non-live slot on the probe path of `hash` (the key is known to be absent).
    #[inline]
    fn free_slot(&self, hash: u64) -> usize {
        let mask = self.index.len() - 1;
        let mut i = hash as usize & mask;
        while self.index[i] < DUMMY {
            i = (i + 1) & mask;
        }
        i
    }

    /// Makes an index sized for the live entries (at most half full after it), dropping the
    /// `DUMMY`s. Positions do not change.
    fn rebuild_index(&mut self) {
        let cap = ((self.live + 1) * 2).next_power_of_two().max(8);
        self.fill_index(cap);
    }

    fn fill_index(&mut self, cap: usize) {
        self.index.clear();
        self.index.resize(cap, EMPTY);
        let mask = cap - 1;
        for (p, e) in self.entries.iter().enumerate() {
            if e.kv.is_some() {
                let mut i = e.hash as usize & mask;
                while self.index[i] != EMPTY {
                    i = (i + 1) & mask;
                }
                self.index[i] = p as u32;
            }
        }
        self.used = self.live;
    }

    /// Removes an entry, returning its key and value.
    #[inline]
    pub fn remove(&mut self, key: &Value) -> Option<(Value, Value)> {
        self.remove_hashed(hash_value(key), key)
    }

    fn remove_hashed(&mut self, hash: u64, key: &Value) -> Option<(Value, Value)> {
        let p = self.find(hash, key)?;
        let removed = self.entries[p].kv.take();
        self.live -= 1;
        // Turn the slot that names `p` into a DUMMY.
        let mask = self.index.len() - 1;
        let mut i = hash as usize & mask;
        while self.index[i] != p as u32 {
            i = (i + 1) & mask;
        }
        self.index[i] = DUMMY;
        if self.entries.len() - self.live > COMPACT_MIN && self.entries.len() > 2 * self.live {
            self.compact();
        }
        removed
    }

    /// Drops the tombstones and renumbers the index.
    fn compact(&mut self) {
        self.entries.retain(|e| e.kv.is_some());
        let cap = ((self.live + 1) * 2).next_power_of_two().max(8);
        self.fill_index(cap);
    }

    /// Tests: how many distinct hashes the entries hold and the most entries sharing one.
    #[cfg(test)]
    fn collision_stats(&self) -> (usize, usize) {
        let mut counts = std::collections::HashMap::new();
        for e in self.entries.iter().filter(|e| e.kv.is_some()) {
            *counts.entry(e.hash).or_insert(0usize) += 1;
        }
        (counts.len(), counts.values().copied().max().unwrap_or(0))
    }

    /// Live entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &(Value, Value)> {
        self.entries.iter().filter_map(|e| e.kv.as_ref())
    }

    /// Removes and returns every entry in insertion order.
    pub(crate) fn take_all(&mut self) -> Vec<(Value, Value)> {
        self.index = Vec::new();
        self.used = 0;
        self.live = 0;
        std::mem::take(&mut self.entries)
            .into_iter()
            .filter_map(|e| e.kv)
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

    fn string(s: &str) -> Value {
        Value::str(s)
    }

    fn keys_of(m: &OrderedMap) -> Vec<Value> {
        m.iter().map(|(k, _)| k.clone()).collect()
    }

    /// The largest number of keys that land on one slot of a power-of-two table of `bits` bits,
    /// by the low bits of the hash (what the inner table uses to pick a slot).
    fn max_slot_load(keys: &[Value], bits: u32) -> usize {
        let mut load = std::collections::HashMap::new();
        for k in keys {
            *load
                .entry(hash_value(k) & ((1 << bits) - 1))
                .or_insert(0usize) += 1;
        }
        load.into_values().max().unwrap_or(0)
    }

    #[test]
    fn many_integer_keys_get_distinct_hashes_and_stay_findable() {
        let mut m = OrderedMap::new();
        for k in 0..100_000 {
            m.insert(int(k), int(-k));
        }
        assert_eq!(m.len(), 100_000);
        // One entry per hash: no bucket needs a second position.
        assert_eq!(m.collision_stats(), (100_000, 1));
        for k in (0..100_000).step_by(7) {
            assert_eq!(m.get(&int(k)), Some(&int(-k)));
        }
        assert_eq!(m.get(&int(100_000)), None);
        assert_eq!(m.get(&int(-1)), None);
    }

    #[test]
    fn many_string_keys_get_distinct_hashes_and_stay_findable() {
        let mut m = OrderedMap::new();
        for k in 0..50_000 {
            m.insert(string(&format!("key{k}")), int(k));
        }
        assert_eq!(m.collision_stats(), (50_000, 1));
        for k in (0..50_000).step_by(11) {
            assert_eq!(m.get(&string(&format!("key{k}"))), Some(&int(k)));
        }
        // Strings that differ only in a short tail, a length, or a trailing NUL.
        let tricky = [
            "",
            "a",
            "a\0",
            "a\0\0",
            "ab",
            "b",
            "aaaaaaaa",
            "aaaaaaaa\0",
            "aaaaaaaab",
        ];
        let hashes: std::collections::HashSet<u64> =
            tricky.iter().map(|s| hash_value(&string(s))).collect();
        assert_eq!(hashes.len(), tricky.len());
    }

    /// The hash of a string as `Value::Str` computes it, without building a value.
    fn hs(s: &str) -> u64 {
        let mut h = KeyHasher::default();
        s.hash(&mut h);
        h.finish()
    }

    /// How many keys of a family share a full 64-bit hash with an earlier one.
    fn full_collisions(hashes: impl Iterator<Item = u64>) -> usize {
        let mut seen = std::collections::HashSet::new();
        hashes.filter(|h| !seen.insert(*h)).count()
    }

    /// Slot load (low bits) and tag spread (top 7 bits) of hashes, as the inner table sees them.
    fn check_spread(name: &str, hashes: &[u64]) {
        let n = hashes.len();
        let bits = (usize::BITS - n.leading_zeros()).clamp(4, 22);
        let mut load = std::collections::HashMap::new();
        for h in hashes {
            *load.entry(h & ((1 << bits) - 1)).or_insert(0usize) += 1;
        }
        let max = load.into_values().max().unwrap_or(0);
        assert!(max <= 16, "{name}: {max} keys on one slot of 2^{bits}");
        let mut tags = [0usize; 128];
        for h in hashes {
            tags[(h >> 57) as usize] += 1;
        }
        let top = tags.into_iter().max().unwrap_or(0);
        assert!(
            top <= n / 128 * 2 + 16,
            "{name}: {top} of {n} keys share one top-7-bit tag"
        );
    }

    #[test]
    fn string_families_have_no_full_hash_collisions_and_spread_over_slots() {
        // (a) the plain counter keys, which collided 46,824 times at 2M with the Fx step.
        let a: Vec<u64> = (0..2_000_000).map(|i| hs(&format!("key{i}"))).collect();
        assert_eq!(full_collisions(a.iter().copied()), 0, "key{{i}}");
        check_spread("key{i}", &a);
        // (b) two varying alphanumerics after a 7-byte prefix: the 8-byte boundary falls
        // between them.
        let alnum: Vec<char> = ('0'..='9').chain('a'..='z').chain('A'..='Z').collect();
        let mut b = Vec::new();
        for x in &alnum {
            for y in &alnum {
                b.push(hs(&format!("abcdefg{x}{y}")));
            }
        }
        assert_eq!(b.len(), 62 * 62);
        assert_eq!(full_collisions(b.iter().copied()), 0, "abcdefg{{x}}{{y}}");
        check_spread("abcdefg{x}{y}", &b);
        // (c) other counter-like names.
        let n = 300_000;
        type Family = (&'static str, fn(usize) -> String);
        let families: [Family; 3] = [
            ("user_{i:06}", |i| format!("user_{i:06}")),
            ("/path/to/file_{i}.txt", |i| {
                format!("/path/to/file_{i}.txt")
            }),
            ("abcd{i:08x}", |i| format!("abcd{i:08x}")),
        ];
        for (name, make) in families {
            let c: Vec<u64> = (0..n).map(|i| hs(&make(i))).collect();
            assert_eq!(full_collisions(c.iter().copied()), 0, "{name}");
            check_spread(name, &c);
        }
        // (d) strings of 9, 15, 16, 17 and 24 bytes that differ in one or two bytes at every
        // position (printable ASCII, so the varying byte is rarely the top byte of a word).
        for len in [9usize, 15, 16, 17, 24] {
            let base = vec![b'a'; len];
            let mut d = Vec::new();
            for p in 0..len {
                for q in p..len {
                    for x in b'0'..=b'z' {
                        for y in *b"0Az" {
                            let mut s = base.clone();
                            s[p] = x;
                            if q != p {
                                s[q] = y;
                            }
                            d.push(String::from_utf8(s).expect("ascii"));
                        }
                    }
                }
            }
            d.sort();
            d.dedup();
            let hashes: Vec<u64> = d.iter().map(|s| hs(s)).collect();
            assert_eq!(
                full_collisions(hashes.iter().copied()),
                0,
                "{len}-byte strings"
            );
        }
        // Dense two-character tails at the end of a 9-byte string (91% collided with Fx).
        let mut tails = Vec::new();
        for x in 0x20u8..0x7f {
            for y in 0x20u8..0x7f {
                tails.push(hs(&format!("abcdefgh{}{}", x as char, y as char)));
            }
        }
        assert_eq!(full_collisions(tails.iter().copied()), 0, "9-byte tails");
        check_spread("9-byte tails", &tails);
    }

    #[test]
    fn integer_pairs_and_struct_like_keys_have_no_full_hash_collisions() {
        // (e) tuples of two integers on a grid, and pairs (i << 32, j).
        let pair = |a: i64, b: i64| hash_value(&Value::tuple(vec![int(a), int(b)]));
        let grid: Vec<u64> = (0..1000)
            .flat_map(|i| (0..1000).map(move |j| (i, j)))
            .map(|(i, j)| pair(i, j))
            .collect();
        assert_eq!(full_collisions(grid.iter().copied()), 0, "1000x1000 grid");
        check_spread("grid", &grid);
        let wide: Vec<u64> = (0..500i64)
            .flat_map(|i| (0..500i64).map(move |j| (i, j)))
            .map(|(i, j)| pair(i << 32, j))
            .collect();
        assert_eq!(full_collisions(wide.iter().copied()), 0, "(i << 32, j)");
        check_spread("(i << 32, j)", &wide);
        // (f) a struct-like key of several small fields, each fed as a word.
        let mut fields = Vec::new();
        for a in 0..12u64 {
            for b in 0..12u64 {
                for c in 0..12u64 {
                    for d in 0..12u64 {
                        let mut h = KeyHasher::default();
                        for w in [a, b, c, d, a ^ d] {
                            h.write_u64(w);
                        }
                        fields.push(h.finish());
                    }
                }
            }
        }
        assert_eq!(full_collisions(fields.iter().copied()), 0, "struct-like");
        check_spread("struct-like", &fields);
    }

    #[test]
    fn sequential_and_power_of_two_multiples_spread_over_the_table() {
        // 2^17 slots for 100k keys: a good hash has a longest chain of about 10; a hash with weak
        // low bits piles every key of a stride onto a few slots.
        let sequential: Vec<Value> = (0..100_000).map(int).collect();
        assert!(max_slot_load(&sequential, 17) <= 16);
        for shift in [4, 8, 16, 24, 32, 40, 48, 56] {
            let strided: Vec<Value> = (0..20_000i64.min(1 << (64 - shift)))
                .map(|i| int(i.wrapping_shl(shift)))
                .collect();
            let load = max_slot_load(&strided, 15);
            assert!(load <= 14, "keys i << {shift}: {load} on one slot");
            // And the high bits (the control tag of the table) vary as well.
            let tags: std::collections::HashSet<u64> =
                strided.iter().map(|k| hash_value(k) >> 57).collect();
            assert!(
                tags.len() >= strided.len().min(128) * 3 / 4,
                "keys i << {shift}: {} tags",
                tags.len()
            );
        }
        let mut m = OrderedMap::new();
        for i in 0..20_000i64 {
            m.insert(int(i << 20), int(i));
        }
        assert_eq!(m.collision_stats(), (20_000, 1));
        assert_eq!(m.get(&int(777 << 20)), Some(&int(777)));
    }

    #[test]
    fn keys_of_different_kinds_do_not_collide_by_kind_alone() {
        let hashes: std::collections::HashSet<u64> = [
            int(1),
            Value::U64(1),
            Value::Bool(true),
            Value::Float(1.0),
            Value::Char('\u{1}'),
            string("\u{1}"),
        ]
        .iter()
        .map(hash_value)
        .collect();
        assert_eq!(hashes.len(), 6);
    }

    #[test]
    fn insertion_order_survives_removals_reinsertions_and_compaction() {
        let mut m = OrderedMap::new();
        for k in 0..300 {
            m.insert(string(&format!("k{k}")), int(k));
        }
        for k in (0..300).filter(|k| k % 3 != 0) {
            assert!(m.remove(&string(&format!("k{k}"))).is_some());
        }
        // 200 removals of 300 compacted the vector; the survivors keep their order.
        let survivors: Vec<Value> = (0..300)
            .filter(|k| k % 3 == 0)
            .map(|k| string(&format!("k{k}")))
            .collect();
        assert_eq!(keys_of(&m), survivors);
        // A removed key comes back at the end; a replaced one keeps its place.
        m.insert(string("k1"), int(1));
        m.insert(string("k0"), int(1000));
        let mut expected = survivors;
        expected.push(string("k1"));
        assert_eq!(keys_of(&m), expected);
        assert_eq!(m.get(&string("k0")), Some(&int(1000)));
        assert_eq!(m.take_all().len(), 101);
        assert_eq!(m.len(), 0);
        assert_eq!(m.get(&string("k0")), None);
    }

    /// A reference model: a vector of pairs in insertion order, searched linearly.
    #[derive(Default)]
    struct Model(Vec<(i64, i64)>);

    impl Model {
        fn insert(&mut self, k: i64, v: i64) -> Option<i64> {
            match self.0.iter_mut().find(|(a, _)| *a == k) {
                Some(e) => Some(std::mem::replace(&mut e.1, v)),
                None => {
                    self.0.push((k, v));
                    None
                }
            }
        }
        fn remove(&mut self, k: i64) -> Option<i64> {
            let i = self.0.iter().position(|(a, _)| *a == k)?;
            Some(self.0.remove(i).1)
        }
        fn get(&self, k: i64) -> Option<i64> {
            self.0.iter().find(|(a, _)| *a == k).map(|e| e.1)
        }
    }

    fn pairs_of(m: &OrderedMap) -> Vec<(i64, i64)> {
        m.iter()
            .map(|(k, v)| match (k, v) {
                (Value::Int(k), Value::Int(v)) => (*k, *v),
                _ => panic!("not ints"),
            })
            .collect()
    }

    /// Checks the index invariants: every live entry is reachable from its own hash, `used`
    /// counts the non-empty slots, and the index is at most two thirds full.
    fn check_invariants(m: &OrderedMap) {
        let non_empty = m.index.iter().filter(|&&s| s != EMPTY).count();
        assert_eq!(non_empty, m.used);
        assert!(m.index.is_empty() || m.used * 3 <= m.index.len() * 2);
        assert_eq!(m.entries.iter().filter(|e| e.kv.is_some()).count(), m.live);
        for (p, e) in m.entries.iter().enumerate() {
            if let Some((k, _)) = &e.kv {
                assert_eq!(m.find(e.hash, k), Some(p));
            }
        }
    }

    /// Deterministic xorshift, so the test is the same on every run.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn random_operations_match_a_reference_model() {
        for (seed, key_space) in [(1u64, 16u64), (2, 100), (3, 1000), (4, 5)] {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed);
            let mut m = OrderedMap::new();
            let mut model = Model::default();
            for step in 0..20_000 {
                let k = (rng.next() % key_space) as i64;
                let v = rng.next() as i64;
                match rng.next() % 10 {
                    0..=4 => {
                        let (old, spare) = m.insert(int(k), int(v));
                        assert_eq!(old, model.insert(k, v).map(int), "insert {k}");
                        assert_eq!(spare.is_some(), old.is_some());
                    }
                    5..=7 => {
                        let got = m.remove(&int(k)).map(|(_, v)| v);
                        assert_eq!(got, model.remove(k).map(int), "remove {k}");
                    }
                    _ => assert_eq!(m.get(&int(k)), model.get(k).map(int).as_ref()),
                }
                assert_eq!(m.len(), model.0.len());
                if step % 997 == 0 {
                    assert_eq!(pairs_of(&m), model.0);
                    check_invariants(&m);
                }
            }
            assert_eq!(pairs_of(&m), model.0);
            check_invariants(&m);
        }
    }

    #[test]
    fn every_key_with_one_hash_is_still_found_removed_and_ordered() {
        // All 300 keys hash alike: one probe run of 300 slots, with removals in the middle.
        let mut m = OrderedMap::new();
        for k in 0..300 {
            assert_eq!(m.insert_hashed(7, int(k), int(k)), (None, None));
        }
        check_invariants(&m);
        for k in 0..300 {
            assert_eq!(m.find(7, &int(k)), Some(k as usize));
        }
        assert_eq!(m.find(7, &int(300)), None);
        // Remove every other key (not through `remove`, so the hash stays 7).
        for k in (0..300).step_by(2) {
            assert_eq!(m.remove_hashed(7, &int(k)), Some((int(k), int(k))));
        }
        for k in (1..300).step_by(2) {
            assert!(
                m.find(7, &int(k)).is_some(),
                "lost {k} behind a removed slot"
            );
        }
        // A replaced key keeps its place; a removed key comes back at the end.
        assert_eq!(m.insert_hashed(7, int(1), int(-1)).0, Some(int(1)));
        m.insert_hashed(7, int(0), int(0));
        assert_eq!(m.iter().last().map(|(k, _)| k.clone()), Some(int(0)));
        assert_eq!(m.iter().next().map(|(k, _)| k.clone()), Some(int(1)));
        check_invariants(&m);
    }

    #[test]
    fn index_grows_and_keeps_every_key_through_each_size() {
        let mut m = OrderedMap::new();
        let mut sizes = std::collections::BTreeSet::new();
        for k in 0..5000 {
            m.insert(int(k), int(k));
            sizes.insert(m.index.len());
            if k % 61 == 0 {
                check_invariants(&m);
            }
        }
        // 8, 16, ... 8192 slots: grown by doubling, never above two thirds full.
        assert!(sizes.len() >= 9, "{sizes:?}");
        assert!(sizes.iter().all(|n| n.is_power_of_two()));
        for k in 0..5000 {
            assert_eq!(m.get(&int(k)), Some(&int(k)));
        }
    }

    #[test]
    fn heavy_churn_without_growth_keeps_the_index_usable() {
        // A window of 20 live keys that slides over 100,000 keys: thousands of removals and
        // insertions. Dummies must be reclaimed (rebuilds or compaction) so that the index
        // never fills and a probe for an absent key always ends.
        let mut m = OrderedMap::new();
        for k in 0..20 {
            m.insert(int(k), int(k));
        }
        for k in 20..100_000 {
            assert!(m.remove(&int(k - 20)).is_some());
            m.insert(int(k), int(k));
            assert_eq!(m.get(&int(k - 20)), None);
            if k % 5003 == 0 {
                check_invariants(&m);
            }
        }
        assert_eq!(m.len(), 20);
        assert!(m.index.len() <= 64, "index grew to {}", m.index.len());
        assert!(m.slots() <= 100, "entries grew to {}", m.slots());
        let keys: Vec<i64> = pairs_of(&m).into_iter().map(|p| p.0).collect();
        assert_eq!(keys, (99_980..100_000).collect::<Vec<_>>());
    }

    #[test]
    fn emptying_and_refilling_a_map_works() {
        let mut m = OrderedMap::new();
        for round in 0..3 {
            for k in 0..100 {
                m.insert(int(k), int(round));
            }
            assert_eq!(m.len(), 100);
            if round == 1 {
                assert_eq!(m.take_all().len(), 100);
            } else {
                for k in 0..100 {
                    assert!(m.remove(&int(k)).is_some());
                }
            }
            assert!(m.is_empty());
            assert_eq!(m.get(&int(5)), None);
            assert_eq!(m.remove(&int(5)), None);
            check_invariants(&m);
        }
    }
}
