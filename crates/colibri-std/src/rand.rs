//! `rand`: a seedable, deterministic random number generator.
//!
//! The algorithm is fixed so that a seed means the same sequence on every platform and in every
//! release of this crate: the seed is expanded to 256 bits of state with SplitMix64, and each
//! number comes from xoshiro256** (Blackman and Vigna, 2018). Floats take the top 53 bits;
//! integer ranges use Lemire's multiply-and-reject method, so every value is equally likely.

use colibri_runtime::{Ctx, Error, HostType, TypeBuilder, Value, Vm};

use crate::config::State;
use crate::plain;

/// A generator (`rand.Rng` in scripts): independent of the module's global one.
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Rng {
    /// The generator for `seed`. Any seed is fine, zero included.
    pub fn from_seed(seed: u64) -> Rng {
        let mut sm = seed;
        Rng {
            s: [
                splitmix64(&mut sm),
                splitmix64(&mut sm),
                splitmix64(&mut sm),
                splitmix64(&mut sm),
            ],
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// A float in `[0, 1)` with 53 random bits.
    pub fn float(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }

    /// An integer in `[0, n)`; `n` must not be zero.
    pub fn below(&mut self, n: u64) -> u64 {
        let threshold = n.wrapping_neg() % n;
        loop {
            let m = u128::from(self.next_u64()) * u128::from(n);
            if (m as u64) >= threshold {
                return (m >> 64) as u64;
            }
        }
    }

    /// An integer in `[lo, hi)`, or `None` when the range is empty.
    pub fn range(&mut self, lo: i64, hi: i64) -> Option<i64> {
        if lo >= hi {
            return None;
        }
        let span = (i128::from(hi) - i128::from(lo)) as u64;
        // Cannot overflow: the offset is below the span.
        Some((i128::from(lo) + i128::from(self.below(span))) as i64)
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.float() < p
    }

    /// Fisher-Yates.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

fn empty_range(cx: &mut Ctx, lo: i64, hi: i64) -> ! {
    cx.raise(format!(
        "rand.int: the range {lo}..{hi} is empty (it needs lo < hi)"
    ))
}

impl HostType for Rng {
    const NAME: &'static str = "Rng";

    fn describe(d: &mut TypeBuilder<Self>) {
        d.static_fn("new", ["seed"], |seed: i64| Rng::from_seed(seed as u64));
        d.method_mut(
            "int",
            ["lo", "hi"],
            |cx: &mut Ctx, this: &mut Rng, lo: i64, hi: i64| {
                this.range(lo, hi)
                    .unwrap_or_else(|| empty_range(cx, lo, hi))
            },
        );
        d.method_mut("float", [], |this: &mut Rng| this.float());
        d.method_mut("bool", [], |this: &mut Rng| this.next_u64() >> 63 == 1);
        d.method_mut("chance", ["p"], |this: &mut Rng, p: f64| this.chance(p));
    }
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("rand");
    m.register_type::<Rng>()?;
    plain(
        m.register_fn_named("seed", ["seed"], |cx: &mut Ctx, seed: i64| {
            if let Some(state) = cx.state_mut::<State>() {
                state.rng = Rng::from_seed(seed as u64);
            }
        }),
    )?;
    plain(
        m.register_fn_named("int", ["lo", "hi"], |cx: &mut Ctx, lo: i64, hi: i64| {
            let drawn = cx
                .state_mut::<State>()
                .and_then(|state| state.rng.range(lo, hi));
            drawn.unwrap_or_else(|| empty_range(cx, lo, hi))
        }),
    )?;
    plain(m.register_fn_named("float", [], |cx: &mut Ctx| {
        cx.state_mut::<State>().map_or(0.0, |s| s.rng.float())
    }))?;
    plain(m.register_fn_named("bool", [], |cx: &mut Ctx| {
        cx.state_mut::<State>()
            .is_some_and(|s| s.rng.next_u64() >> 63 == 1)
    }))?;
    plain(
        m.register_fn_named("chance", ["p"], |cx: &mut Ctx, p: f64| {
            cx.state_mut::<State>().is_some_and(|s| s.rng.chance(p))
        }),
    )?;
    // `choice` and `shuffled` work on a list of anything.
    plain(m.register_fn_named(
        "choice",
        ["items"],
        |cx: &mut Ctx, items: Vec<Value>| -> Option<Value> {
            if items.is_empty() {
                return None;
            }
            let state = cx.state_mut::<State>()?;
            let i = state.rng.below(items.len() as u64) as usize;
            items.into_iter().nth(i)
        },
    ))?;
    plain(m.register_fn_named(
        "shuffled",
        ["items"],
        |cx: &mut Ctx, mut items: Vec<Value>| -> Vec<Value> {
            if let Some(state) = cx.state_mut::<State>() {
                state.rng.shuffle(&mut items);
            }
            items
        },
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sequence_for_a_seed_is_fixed() {
        // Reference values: if these change, every saved replay changes.
        let mut rng = Rng::from_seed(42);
        let first: Vec<u64> = (0..3).map(|_| rng.next_u64()).collect();
        assert_eq!(first, REFERENCE_42);
        let mut zero = Rng::from_seed(0);
        assert_eq!(zero.next_u64(), 11_091_344_671_253_066_420);
        assert_eq!(zero.next_u64(), 13_793_997_310_169_335_082);
    }

    // Computed with an independent implementation (Python) of SplitMix64 and xoshiro256**.
    const REFERENCE_42: [u64; 3] = [
        1_546_998_764_402_558_742,
        6_990_951_692_964_543_102,
        12_544_586_762_248_559_009,
    ];

    #[test]
    fn ranges_are_unbiased_and_in_bounds() {
        let mut rng = Rng::from_seed(7);
        let mut counts = [0u32; 6];
        for _ in 0..6000 {
            let v = rng.range(1, 7).unwrap();
            counts[(v - 1) as usize] += 1;
        }
        assert!(
            counts.iter().all(|&c| (800..1200).contains(&c)),
            "{counts:?}"
        );
        assert_eq!(rng.range(5, 5), None);
        assert!(rng.range(i64::MIN, i64::MAX).is_some());
    }

    #[test]
    fn floats_are_in_the_unit_interval() {
        let mut rng = Rng::from_seed(1);
        assert!(
            (0..1000)
                .map(|_| rng.float())
                .all(|f| (0.0..1.0).contains(&f))
        );
    }
}
