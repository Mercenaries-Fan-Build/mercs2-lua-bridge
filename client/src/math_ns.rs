//! The `math` additions: single-precision trig, log, and random functions the game's Lua build does
//! not ship, registered into its existing `math` table.
//!
//! Every function computes in `f32`, the game's `lua_Number`. Arguments must be Lua numbers (a
//! numeric string is refused); a missing or mistyped argument raises
//! `bad argument #N to 'name' (number expected, got T)`. A value used as an integer is truncated
//! toward zero and must fit an `i32` (or a `u32` for `randomseed`), else it raises.
//!
//! The random generator is the C runtime's: a per-thread linear congruential generator
//! (`state = state * 214013 + 2531011`, output `(state >> 16) & 0x7fff`, `RAND_MAX` 32767), seeded
//! with 1 on each thread until `math.randomseed` sets it.

use crate::lua_api::{Lua, Ret, TNUMBER, type_name};
use std::cell::Cell;

/// The names this module registers into `math`, in registration order.
pub const MATH_FUNCTIONS: &[&str] = &[
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "sinh",
    "cosh",
    "tanh",
    "sqrt",
    "log",
    "log10",
    "fmod",
    "ldexp",
    "modf",
    "frexp",
    "random",
    "randomseed",
    "FreedomUnits",
];

/// `RAND_MAX` of the C runtime generator.
pub const RAND_MAX: i32 = 0x7fff;

/// Metres to yards.
const YARDS_PER_METRE: f32 = 1.09361;

thread_local! {
    static RAND_STATE: Cell<u32> = const { Cell::new(1) };
}

/// The next value of this thread's generator, in `0..=RAND_MAX`.
pub fn crt_rand() -> i32 {
    RAND_STATE.with(|s| {
        let next = s.get().wrapping_mul(214_013).wrapping_add(2_531_011);
        s.set(next);
        ((next >> 16) & 0x7fff) as i32
    })
}

/// Seed this thread's generator.
pub fn crt_srand(seed: u32) {
    RAND_STATE.with(|s| s.set(seed));
}

fn bad_arg(i: i32, fname: &str, msg: &str) -> String {
    format!("bad argument #{} to '{fname}' ({msg})", i + 1)
}

fn check_number<L: Lua>(l: &L, i: i32, fname: &str) -> Result<f32, String> {
    let tag = l.arg_type(i);
    if tag != TNUMBER {
        return Err(bad_arg(i, fname, &format!("number expected, got {}", type_name(tag))));
    }
    match l.arg_number(i) {
        Some(n) => Ok(n as f32),
        None => Err(bad_arg(i, fname, "number expected, got an unreadable number")),
    }
}

/// `(int)x`: truncation toward zero, refused when the result does not fit an `i32`.
fn check_int<L: Lua>(l: &L, i: i32, fname: &str) -> Result<i32, String> {
    let x = check_number(l, i, fname)?;
    let t = x.trunc();
    if t.is_finite() && (-2_147_483_648.0..2_147_483_648.0).contains(&t) {
        Ok(t as i32)
    } else {
        Err(bad_arg(i, fname, &format!("{x} does not fit a 32-bit integer")))
    }
}

fn values(l: &mut impl Lua, out: &[f32]) -> Ret {
    for &v in out {
        l.push_number(f64::from(v));
    }
    Ret::Values(out.len() as i32)
}

fn one_arg<L: Lua>(l: &mut L, fname: &str, f: fn(f32) -> f32) -> Ret {
    match check_number(l, 0, fname) {
        Ok(x) => values(l, &[f(x)]),
        Err(e) => Ret::Raise(e),
    }
}

fn two_arg<L: Lua>(l: &mut L, fname: &str, f: fn(f32, f32) -> f32) -> Ret {
    let x = match check_number(l, 0, fname) {
        Ok(x) => x,
        Err(e) => return Ret::Raise(e),
    };
    match check_number(l, 1, fname) {
        Ok(y) => values(l, &[f(x, y)]),
        Err(e) => Ret::Raise(e),
    }
}

pub fn sin<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "sin", f32::sin)
}
pub fn cos<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "cos", f32::cos)
}
pub fn tan<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "tan", f32::tan)
}
pub fn asin<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "asin", f32::asin)
}
pub fn acos<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "acos", f32::acos)
}
pub fn atan<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "atan", f32::atan)
}
pub fn sinh<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "sinh", f32::sinh)
}
pub fn cosh<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "cosh", f32::cosh)
}
pub fn tanh<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "tanh", f32::tanh)
}
pub fn sqrt<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "sqrt", f32::sqrt)
}
pub fn log<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "log", f32::ln)
}
pub fn log10<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "log10", f32::log10)
}
/// `math.FreedomUnits(m)`: metres to yards.
pub fn freedom_units<L: Lua>(l: &mut L) -> Ret {
    one_arg(l, "FreedomUnits", |m| m * YARDS_PER_METRE)
}
pub fn atan2<L: Lua>(l: &mut L) -> Ret {
    two_arg(l, "atan2", f32::atan2)
}
/// `fmodf`: the remainder of `x / y` with the sign of `x`.
pub fn fmod<L: Lua>(l: &mut L) -> Ret {
    two_arg(l, "fmod", |x, y| x % y)
}

/// `ldexpf(x, e)`, correctly rounded once to `f32`.
pub fn ldexp_f32(x: f32, e: i32) -> f32 {
    // Past +-400 every finite non-zero f32 has already overflowed or underflowed, and the f64
    // product of an f32 and a power of two in that range is exact, so the one rounding is the cast.
    let e = e.clamp(-400, 400);
    (f64::from(x) * 2f64.powi(e)) as f32
}

/// `math.ldexp(x, e)`; `e` is truncated toward zero.
pub fn ldexp<L: Lua>(l: &mut L) -> Ret {
    let x = match check_number(l, 0, "ldexp") {
        Ok(x) => x,
        Err(e) => return Ret::Raise(e),
    };
    match check_int(l, 1, "ldexp") {
        Ok(e) => values(l, &[ldexp_f32(x, e)]),
        Err(e) => Ret::Raise(e),
    }
}

/// `modff(x)`: the integer part and the fractional part, both carrying the sign of `x`.
pub fn modf_f32(x: f32) -> (f32, f32) {
    if x.is_infinite() {
        (x, 0f32.copysign(x))
    } else {
        let i = x.trunc();
        (i, (x - i).copysign(x))
    }
}

/// `math.modf(x)` -> integer part, fractional part.
pub fn modf<L: Lua>(l: &mut L) -> Ret {
    match check_number(l, 0, "modf") {
        Ok(x) => {
            let (i, f) = modf_f32(x);
            values(l, &[i, f])
        }
        Err(e) => Ret::Raise(e),
    }
}

/// `frexpf(x)` for finite `x`: a mantissa in `[0.5, 1)` (or zero) and the power of two.
pub fn frexp_f32(x: f32) -> (f32, i32) {
    if x == 0.0 {
        return (x, 0);
    }
    let bits = x.to_bits();
    let exp_field = ((bits >> 23) & 0xff) as i32;
    if exp_field == 0 {
        // Subnormal: scale into the normal range exactly, then undo the scale.
        let (m, e) = frexp_f32(x * 33_554_432.0);
        return (m, e - 25);
    }
    let mant = f32::from_bits((bits & 0x807f_ffff) | (126 << 23));
    (mant, exp_field - 126)
}

/// `math.frexp(x)` -> mantissa, exponent. The exponent of an infinity or NaN is unspecified by C,
/// so those raise.
pub fn frexp<L: Lua>(l: &mut L) -> Ret {
    match check_number(l, 0, "frexp") {
        Ok(x) if x.is_finite() => {
            let (m, e) = frexp_f32(x);
            values(l, &[m, e as f32])
        }
        Ok(x) => Ret::Raise(bad_arg(0, "frexp", &format!("{x} has no exponent"))),
        Err(e) => Ret::Raise(e),
    }
}

/// `math.random()` -> a float in `[0, 1)`; `math.random(n)` -> an integer `1 + rand() % n`
/// (`n >= 1`); `math.random(m, n)` -> an integer `m + rand() % (n - m + 1)` (`m <= n`).
pub fn random<L: Lua>(l: &mut L) -> Ret {
    let nargs = (0..3).take_while(|&i| l.arg_type(i) != -1).count();
    let result = match nargs {
        0 => (crt_rand() % RAND_MAX) as f32 / RAND_MAX as f32,
        1 => {
            let upper = match check_int(l, 0, "random") {
                Ok(u) => u,
                Err(e) => return Ret::Raise(e),
            };
            if upper < 1 {
                return Ret::Raise(bad_arg(0, "random", "interval is empty"));
            }
            (1 + crt_rand() % upper) as f32
        }
        2 => {
            let lo = match check_int(l, 0, "random") {
                Ok(v) => v,
                Err(e) => return Ret::Raise(e),
            };
            let hi = match check_int(l, 1, "random") {
                Ok(v) => v,
                Err(e) => return Ret::Raise(e),
            };
            if hi < lo {
                return Ret::Raise(bad_arg(1, "random", "interval is empty"));
            }
            let span = i64::from(hi) - i64::from(lo) + 1;
            (i64::from(lo) + i64::from(crt_rand()) % span) as f32
        }
        _ => return Ret::Raise("wrong number of arguments to 'random'".to_string()),
    };
    values(l, &[result])
}

/// `math.randomseed(x)`: seed the generator with `x` truncated to an unsigned 32-bit integer.
pub fn randomseed<L: Lua>(l: &mut L) -> Ret {
    let x = match check_number(l, 0, "randomseed") {
        Ok(x) => x,
        Err(e) => return Ret::Raise(e),
    };
    let t = x.trunc();
    if !(t.is_finite() && (0.0..4_294_967_296.0).contains(&t)) {
        return Ret::Raise(bad_arg(
            0,
            "randomseed",
            &format!("{x} does not fit an unsigned 32-bit integer"),
        ));
    }
    crt_srand(t as u32);
    Ret::Values(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_api::fake::{FakeLua, Val, s};

    fn call(f: Binding, args: Vec<Val>) -> (Ret, Vec<Val>) {
        let mut l = FakeLua::with(args);
        let r = f(&mut l);
        (r, l.pushed)
    }

    fn num(f: Binding, args: Vec<Val>) -> Vec<f32> {
        let (r, pushed) = call(f, args);
        assert_eq!(r, Ret::Values(pushed.len() as i32));
        pushed
            .into_iter()
            .map(|v| match v {
                Val::Num(n) => n as f32,
                other => panic!("expected a number, got {other:?}"),
            })
            .collect()
    }

    fn raises(f: Binding, args: Vec<Val>) -> String {
        match call(f, args) {
            (Ret::Raise(m), pushed) => {
                assert!(pushed.is_empty());
                m
            }
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    type Binding = fn(&mut FakeLua) -> Ret;
    type F32Fn = fn(f32) -> f32;

    #[test]
    fn one_arg_functions_compute_in_f32() {
        let x = 0.5f32;
        let cases: [(Binding, F32Fn); 12] = [
            (sin, f32::sin),
            (cos, f32::cos),
            (tan, f32::tan),
            (asin, f32::asin),
            (acos, f32::acos),
            (atan, f32::atan),
            (sinh, f32::sinh),
            (cosh, f32::cosh),
            (tanh, f32::tanh),
            (sqrt, f32::sqrt),
            (log, f32::ln),
            (log10, f32::log10),
        ];
        for (binding, f) in cases {
            assert_eq!(num(binding, vec![Val::Num(0.5)]), vec![f(x)]);
        }
        assert_eq!(num(sqrt, vec![Val::Num(16.0)]), vec![4.0]);
    }

    #[test]
    fn freedom_units_is_metres_times_1_09361() {
        assert_eq!(num(freedom_units, vec![Val::Num(100.0)]), vec![100.0f32 * 1.09361f32]);
    }

    #[test]
    fn two_arg_functions() {
        assert_eq!(num(atan2, vec![Val::Num(1.0), Val::Num(1.0)]), vec![1f32.atan2(1.0)]);
        assert_eq!(num(fmod, vec![Val::Num(-7.0), Val::Num(3.0)]), vec![-1.0]);
        assert_eq!(num(fmod, vec![Val::Num(7.5), Val::Num(2.0)]), vec![1.5]);
    }

    #[test]
    fn non_number_arguments_raise() {
        assert_eq!(
            raises(sin, vec![]),
            "bad argument #1 to 'sin' (number expected, got no value)"
        );
        assert_eq!(
            raises(sqrt, vec![s("4")]),
            "bad argument #1 to 'sqrt' (number expected, got string)"
        );
        assert_eq!(
            raises(atan2, vec![Val::Num(1.0)]),
            "bad argument #2 to 'atan2' (number expected, got no value)"
        );
        assert_eq!(
            raises(fmod, vec![Val::Num(1.0), Val::Nil]),
            "bad argument #2 to 'fmod' (number expected, got nil)"
        );
        assert!(raises(ldexp, vec![Val::Num(1.0), Val::Bool(true)]).contains("got boolean"));
        assert!(raises(modf, vec![Val::Table]).contains("got table"));
        assert!(raises(frexp, vec![]).contains("got no value"));
        assert!(raises(randomseed, vec![s("1")]).contains("got string"));
    }

    #[test]
    fn ldexp_truncates_the_exponent_and_rounds_once() {
        assert_eq!(num(ldexp, vec![Val::Num(0.75), Val::Num(4.9)]), vec![12.0]);
        assert_eq!(
            num(ldexp, vec![Val::Num(1.0), Val::Num(-149.0)]),
            vec![f32::from_bits(1)]
        );
        assert_eq!(num(ldexp, vec![Val::Num(1.0), Val::Num(128.0)]), vec![f32::INFINITY]);
        assert_eq!(ldexp_f32(1.5, -150), f32::from_bits(1));
        assert_eq!(ldexp_f32(f32::MAX, -1000), 0.0);
        assert_eq!(ldexp_f32(f32::from_bits(1), 1000), f32::INFINITY);
        assert!(raises(ldexp, vec![Val::Num(1.0), Val::Num(f64::INFINITY)]).contains("32-bit integer"));
        assert!(raises(ldexp, vec![Val::Num(1.0), Val::Num(3e9)]).contains("32-bit integer"));
    }

    #[test]
    fn modf_returns_integer_then_fraction_with_sign() {
        assert_eq!(num(modf, vec![Val::Num(3.75)]), vec![3.0, 0.75]);
        let neg = num(modf, vec![Val::Num(-2.5)]);
        assert_eq!(neg, vec![-2.0, -0.5]);
        let (i, f) = modf_f32(-2.0);
        assert_eq!((i, f.to_bits()), (-2.0, (-0.0f32).to_bits()));
        assert_eq!(modf_f32(f32::NEG_INFINITY).0, f32::NEG_INFINITY);
        assert_eq!(modf_f32(f32::NEG_INFINITY).1.to_bits(), (-0.0f32).to_bits());
    }

    #[test]
    fn frexp_returns_mantissa_then_exponent() {
        assert_eq!(num(frexp, vec![Val::Num(8.0)]), vec![0.5, 4.0]);
        assert_eq!(num(frexp, vec![Val::Num(-3.0)]), vec![-0.75, 2.0]);
        assert_eq!(num(frexp, vec![Val::Num(0.0)]), vec![0.0, 0.0]);
        assert_eq!(frexp_f32(f32::from_bits(1)), (0.5, -148));
        assert_eq!(frexp_f32(f32::MAX).1, 128);
        assert!(raises(frexp, vec![Val::Num(f64::INFINITY)]).contains("no exponent"));
    }

    #[test]
    fn crt_generator_matches_the_c_runtime_sequence() {
        crt_srand(1);
        assert_eq!(
            [crt_rand(), crt_rand(), crt_rand(), crt_rand()],
            [41, 18467, 6334, 26500]
        );
        crt_srand(42);
        let first = crt_rand();
        crt_srand(42);
        assert_eq!(crt_rand(), first);
    }

    #[test]
    fn random_without_arguments_is_in_zero_one_half_open() {
        crt_srand(7);
        for _ in 0..100_000 {
            let v = num(random, vec![])[0];
            assert!((0.0..1.0).contains(&v), "{v}");
        }
        // rand() == RAND_MAX maps to 0, not 1.
        let state_before_max = (0u32..=u32::MAX)
            .find(|&st| ((st.wrapping_mul(214_013).wrapping_add(2_531_011) >> 16) & 0x7fff) == 0x7fff)
            .expect("some state yields RAND_MAX");
        crt_srand(state_before_max);
        assert_eq!(num(random, vec![]), vec![0.0]);
    }

    #[test]
    fn random_with_bounds_uses_modulo_of_rand() {
        crt_srand(1);
        assert_eq!(num(random, vec![Val::Num(10.0)]), vec![(1 + 41 % 10) as f32]);
        assert_eq!(
            num(random, vec![Val::Num(-5.0), Val::Num(5.0)]),
            vec![(-5 + 18467 % 11) as f32]
        );
        crt_srand(1);
        assert_eq!(num(random, vec![Val::Num(10.9)]), vec![2.0]);
        for _ in 0..10_000 {
            let v = num(random, vec![Val::Num(3.0), Val::Num(4.0)])[0];
            assert!(v == 3.0 || v == 4.0);
        }
        assert_eq!(num(random, vec![Val::Num(7.0), Val::Num(7.0)]), vec![7.0]);
    }

    #[test]
    fn random_rejects_empty_intervals_and_extra_arguments() {
        assert_eq!(
            raises(random, vec![Val::Num(0.0)]),
            "bad argument #1 to 'random' (interval is empty)"
        );
        assert_eq!(
            raises(random, vec![Val::Num(5.0), Val::Num(4.0)]),
            "bad argument #2 to 'random' (interval is empty)"
        );
        assert_eq!(
            raises(random, vec![Val::Num(1.0), Val::Num(2.0), Val::Num(3.0)]),
            "wrong number of arguments to 'random'"
        );
        assert!(raises(random, vec![Val::Nil]).contains("got nil"));
        assert!(raises(random, vec![Val::Num(f64::NAN)]).contains("32-bit integer"));
    }

    #[test]
    fn random_full_i32_span_does_not_overflow() {
        let v = num(random, vec![Val::Num(-2_147_483_648.0), Val::Num(2_147_483_520.0)])[0];
        assert!(v >= -2_147_483_648.0);
    }

    #[test]
    fn randomseed_seeds_the_generator() {
        let (r, pushed) = call(randomseed, vec![Val::Num(1.9)]);
        assert_eq!(r, Ret::Values(0));
        assert!(pushed.is_empty());
        assert_eq!(crt_rand(), 41);
        assert!(raises(randomseed, vec![Val::Num(-1.0)]).contains("unsigned 32-bit"));
        assert!(raises(randomseed, vec![Val::Num(f64::NAN)]).contains("unsigned 32-bit"));
    }

    #[test]
    fn function_names_are_the_registered_set() {
        assert_eq!(MATH_FUNCTIONS.len(), 20);
        assert!(MATH_FUNCTIONS.contains(&"FreedomUnits"));
    }
}
