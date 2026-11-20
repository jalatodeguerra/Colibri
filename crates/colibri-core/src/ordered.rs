//! An insertion-ordered hash map of values, the storage of `Map` and `Set` (DESIGN §5.2).
//!
//! Entries live in a vector in insertion order; a hash index maps the hash of a key to the
//! positions of the entries with that hash. Removing an entry leaves a tombstone so the other
//! positions stay valid (an iteration holds positions while the mutation guard blocks any
//! removal); the vector is compacted when tombstones outnumber live entries. Lookup, insert and
//! remove are O(1) on average. Keys are compared with the structural equality of
//! [`crate::compare::values_equal`], so a key is stored once, in the entry, and the index holds
//! no second reference to it.
//!
//! This module is pure data structure code: it never runs Colibri code.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
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
pub fn hash_value(v: &Value) -> u64 {
    hash_value_with(v, &mut |_| None)
}

/// [`hash_value`], where `host` may give the hash of an object of a host class (at any depth)
/// in place of its handle. Used for the `hash()` that scripts see: an object that a host class
/// compares by its own `==` must hash by what that `==` looks at.
pub fn hash_value_with(v: &Value, host: &mut dyn FnMut(&Value) -> Option<u64>) -> u64 {
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
/// equal. The inner table below uses the hash as it is, so [`KeyHasher::finish`] runs a
/// full-avalanche finalizer (the MurmurHash3 64-bit mix) to spread the low bits that pick a
/// table slot and the high bits that tag it. The hash is not a secret: `x.hash()` returns it
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

/// The inner table's hasher: the key is already a well-mixed 64-bit hash, so it is used as it
/// is instead of being hashed a second time.
#[derive(Default)]
struct PassThrough(u64);

impl Hasher for PassThrough {
    fn write(&mut self, bytes: &[u8]) {
        // Only `u64` keys are ever used; fold anything else in so the hasher stays correct.
        for b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*b);
        }
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type IndexMap = HashMap<u64, Bucket, BuildHasherDefault<PassThrough>>;

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
    index: IndexMap,
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

    /// How many positions exist, live or removed: the bound of [`OrderedMap::entry_at`].
    #[inline]
    pub fn slots(&self) -> usize {
        self.entries.len()
    }

    /// The entry at a position, if it is live.
    #[inline]
    pub fn entry_at(&self, slot: usize) -> Option<&(Value, Value)> {
        self.entries.get(slot).and_then(Option::as_ref)
    }

    #[inline]
    fn find(&self, hash: u64, key: &Value) -> Option<usize> {
        let bucket = self.index.get(&hash)?;
        bucket.positions().iter().copied().find(|&p| {
            self.entries[p]
                .as_ref()
                .is_some_and(|(k, _)| equal_or_give_up(k, key))
        })
    }

    #[inline]
    pub fn get(&self, key: &Value) -> Option<&Value> {
        let p = self.find(hash_value(key), key)?;
        self.entries[p].as_ref().map(|(_, v)| v)
    }

    #[inline]
    pub fn get_mut(&mut self, key: &Value) -> Option<&mut Value> {
        let p = self.find(hash_value(key), key)?;
        self.entries[p].as_mut().map(|(_, v)| v)
    }

    #[inline]
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

    /// Tests: how many distinct hashes the index holds and the most entries sharing one.
    #[cfg(test)]
    fn collision_stats(&self) -> (usize, usize) {
        let longest = self
            .index
            .values()
            .map(|b| b.positions().len())
            .max()
            .unwrap_or(0);
        (self.index.len(), longest)
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
}
