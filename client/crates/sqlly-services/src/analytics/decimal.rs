//! Exact decimal numbers for values that must keep every digit: bigint ids,
//! `decimal(38, s)` text, and arithmetic over them. Up to 38 significant
//! digits and a scale of 38, as SQL `decimal(38, s)`; anything wider is not
//! an exact decimal here, and callers fall back to floats and say so.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// Most significant digits (and largest scale) an exact decimal holds.
pub const MAX_DIGITS: u32 = 38;

/// `mantissa / 10^scale`, with `|mantissa| < 10^38`. Equality, ordering
/// and hashing are numeric: `1.50` equals `1.5`.
#[derive(Debug, Clone, Copy)]
pub struct Decimal {
    mantissa: i128,
    scale: u32,
}

fn pow10(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

fn limit() -> i128 {
    10i128.pow(MAX_DIGITS)
}

impl Decimal {
    pub const ZERO: Decimal = Decimal {
        mantissa: 0,
        scale: 0,
    };

    /// `mantissa / 10^scale`, `None` past 38 digits or a scale of 38.
    pub fn new(mantissa: i128, scale: u32) -> Option<Self> {
        (scale <= MAX_DIGITS && mantissa.unsigned_abs() < limit().unsigned_abs())
            .then_some(Decimal { mantissa, scale })
    }

    pub fn from_i64(n: i64) -> Self {
        Decimal {
            mantissa: i128::from(n),
            scale: 0,
        }
    }

    /// A plain decimal literal (optional sign, digits, optional fraction,
    /// optional exponent), `None` when it is not one or needs more than 38
    /// digits.
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        let (negative, body) = match trimmed.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
        };
        let (mantissa, exponent) = match body.split_once(['e', 'E']) {
            Some((m, e)) => (m, Some(e)),
            None => (body, None),
        };
        let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        if (int_part.is_empty() && frac_part.is_empty())
            || !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let exponent: i64 = match exponent {
            Some(e) => {
                let digits = e.strip_prefix(['-', '+']).unwrap_or(e);
                if digits.is_empty()
                    || digits.len() > 6
                    || !digits.bytes().all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                e.parse().ok()?
            }
            None => 0,
        };
        let mut digits: String = format!("{int_part}{frac_part}")
            .trim_start_matches('0')
            .to_string();
        let mut scale = frac_part.len() as i64 - exponent;
        if digits.is_empty() {
            return Some(Decimal::ZERO);
        }
        if scale < 0 {
            if digits.len() as i64 - scale > i64::from(MAX_DIGITS) {
                return None;
            }
            digits.extend(std::iter::repeat_n('0', (-scale) as usize));
            scale = 0;
        }
        while (scale > i64::from(MAX_DIGITS) || digits.len() > MAX_DIGITS as usize)
            && scale > 0
            && digits.ends_with('0')
        {
            digits.pop();
            scale -= 1;
        }
        if scale > i64::from(MAX_DIGITS) || digits.len() > MAX_DIGITS as usize {
            return None;
        }
        let magnitude: i128 = digits.parse().ok()?;
        Decimal::new(if negative { -magnitude } else { magnitude }, scale as u32)
    }

    /// The exact value of a finite float's shortest round-tripping text.
    pub fn from_f64(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        Decimal::parse(&format!("{value:e}"))
    }

    pub fn mantissa(self) -> i128 {
        self.mantissa
    }

    pub fn scale(self) -> u32 {
        self.scale
    }

    pub fn is_zero(self) -> bool {
        self.mantissa == 0
    }

    pub fn is_negative(self) -> bool {
        self.mantissa < 0
    }

    /// Trailing fractional zeros dropped (`1.50` → `1.5`, `2.0` → `2`).
    pub fn normalized(self) -> Self {
        let mut out = self;
        while out.scale > 0 && out.mantissa % 10 == 0 {
            out.mantissa /= 10;
            out.scale -= 1;
        }
        out
    }

    pub fn is_integer(self) -> bool {
        self.normalized().scale == 0
    }

    /// The value as an `i64` when it is a whole number in range.
    pub fn to_i64(self) -> Option<i64> {
        let n = self.normalized();
        if n.scale == 0 {
            i64::try_from(n.mantissa).ok()
        } else {
            None
        }
    }

    /// The nearest float (correctly rounded).
    pub fn to_f64(self) -> f64 {
        self.to_string().parse().unwrap_or(f64::NAN)
    }

    /// Whether [`Decimal::to_f64`] keeps the exact value.
    pub fn fits_f64(self) -> bool {
        Decimal::from_f64(self.to_f64()).is_some_and(|back| back == self)
    }

    fn with_scale(self, scale: u32) -> Option<Self> {
        if scale < self.scale {
            return None;
        }
        let mantissa = self.mantissa.checked_mul(pow10(scale - self.scale)?)?;
        Decimal::new(mantissa, scale)
    }

    fn aligned(self, other: Self) -> Option<(i128, i128, u32)> {
        let scale = self.scale.max(other.scale);
        match (self.with_scale(scale), other.with_scale(scale)) {
            (Some(a), Some(b)) => Some((a.mantissa, b.mantissa, scale)),
            _ => {
                let a = self.normalized();
                let b = other.normalized();
                let scale = a.scale.max(b.scale);
                Some((
                    a.with_scale(scale)?.mantissa,
                    b.with_scale(scale)?.mantissa,
                    scale,
                ))
            }
        }
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        let (a, b, scale) = self.aligned(other)?;
        Decimal::new(a.checked_add(b)?, scale)
    }

    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.checked_add(-other)
    }

    pub fn checked_mul(self, other: Self) -> Option<Self> {
        let a = self.normalized();
        let b = other.normalized();
        Decimal::new(a.mantissa.checked_mul(b.mantissa)?, a.scale + b.scale)
    }

    /// The remainder of truncating division (sign of `self`, like SQL `%`);
    /// `None` for a zero divisor.
    pub fn checked_rem(self, other: Self) -> Option<Self> {
        if other.is_zero() {
            return None;
        }
        let (a, b, scale) = self.aligned(other)?;
        Decimal::new(a.checked_rem(b)?, scale)
    }

    pub fn abs(self) -> Self {
        Decimal {
            mantissa: self.mantissa.abs(),
            scale: self.scale,
        }
    }

    pub fn signum(self) -> i64 {
        self.mantissa.signum() as i64
    }

    /// Rounded to `digits` fractional digits (negative rounds to tens,
    /// hundreds, …), halves away from zero as SQL `ROUND`.
    pub fn round(self, digits: i32) -> Option<Self> {
        self.shorten(digits, |q, r, unit| {
            if r.unsigned_abs() * 2 >= unit.unsigned_abs() {
                q + r.signum()
            } else {
                q
            }
        })
    }

    pub fn floor(self) -> Option<Self> {
        self.shorten(0, |q, r, _| if r < 0 { q - 1 } else { q })
    }

    pub fn ceil(self) -> Option<Self> {
        self.shorten(0, |q, r, _| if r > 0 { q + 1 } else { q })
    }

    /// Cut to `digits` fractional digits, `pick(quotient, remainder, unit)`
    /// choosing the kept quotient.
    fn shorten(self, digits: i32, pick: impl Fn(i128, i128, i128) -> i128) -> Option<Self> {
        let digits = digits.clamp(-(MAX_DIGITS as i32), MAX_DIGITS as i32);
        if digits >= 0 && self.scale <= digits as u32 {
            return Some(self);
        }
        let drop = self.scale as i64 - i64::from(digits);
        if drop > i64::from(MAX_DIGITS) {
            return Some(Decimal::ZERO);
        }
        let unit = pow10(drop as u32)?;
        let q = pick(self.mantissa / unit, self.mantissa % unit, unit);
        if digits >= 0 {
            Decimal::new(q, digits as u32)
        } else {
            Decimal::new(q.checked_mul(pow10((-digits) as u32)?)?, 0)
        }
    }
}

impl std::ops::Neg for Decimal {
    type Output = Decimal;

    fn neg(self) -> Decimal {
        Decimal {
            mantissa: -self.mantissa,
            scale: self.scale,
        }
    }
}

impl PartialEq for Decimal {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Decimal {}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        // Whole parts first, then the fractions at a shared scale; neither
        // step can overflow at 38 digits.
        let unit_a = pow10(self.scale).unwrap_or(1);
        let unit_b = pow10(other.scale).unwrap_or(1);
        let whole = (self.mantissa / unit_a).cmp(&(other.mantissa / unit_b));
        if whole != Ordering::Equal {
            return whole;
        }
        let scale = self.scale.max(other.scale);
        let frac_a = (self.mantissa % unit_a) * pow10(scale - self.scale).unwrap_or(1);
        let frac_b = (other.mantissa % unit_b) * pow10(scale - other.scale).unwrap_or(1);
        frac_a.cmp(&frac_b)
    }
}

impl Hash for Decimal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let n = self.normalized();
        n.mantissa.hash(state);
        n.scale.hash(state);
    }
}

impl fmt::Display for Decimal {
    /// Every digit, at the decimal's own scale (`12.50` stays `12.50`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.mantissa.unsigned_abs().to_string();
        let scale = self.scale as usize;
        let sign = if self.mantissa < 0 { "-" } else { "" };
        if scale == 0 {
            return write!(f, "{sign}{digits}");
        }
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (whole, frac) = padded.split_at(padded.len() - scale);
        write!(f, "{sign}{whole}.{frac}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(text: &str) -> Decimal {
        Decimal::parse(text).unwrap()
    }

    #[test]
    fn parses_and_prints_every_digit() {
        assert_eq!(
            d("12345678901234567.89").to_string(),
            "12345678901234567.89"
        );
        assert_eq!(d("-0.05").to_string(), "-0.05");
        assert_eq!(d("+12.50").to_string(), "12.50");
        assert_eq!(d("1.5e3").to_string(), "1500");
        assert_eq!(d("25e-1").to_string(), "2.5");
        assert_eq!(d("000").to_string(), "0");
        assert_eq!(d("1234567890123456789").to_i64(), Some(1234567890123456789));
        assert_eq!(
            d("99999999999999999999999999999999999999")
                .to_string()
                .len(),
            38
        );
        assert!(Decimal::parse("999999999999999999999999999999999999999").is_none());
        assert!(Decimal::parse("1e400").is_none());
        for bad in ["", "-", ".", "1,000", "NaN", "inf", "1e", "$1", "1.2.3"] {
            assert!(Decimal::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn arithmetic_is_exact_and_checked() {
        assert_eq!(
            d("12345678901234567.89")
                .checked_add(Decimal::ZERO)
                .unwrap()
                .to_string(),
            "12345678901234567.89"
        );
        assert_eq!(d("0.1").checked_add(d("0.2")).unwrap(), d("0.3"));
        assert_eq!(
            Decimal::from_i64(9007199254740993)
                .checked_add(Decimal::from_i64(1))
                .unwrap()
                .to_string(),
            "9007199254740994"
        );
        assert_eq!(
            d("1.5").checked_mul(d("-2.25")).unwrap().to_string(),
            "-3.375"
        );
        assert_eq!(d("7").checked_rem(d("-3")).unwrap(), d("1"));
        assert_eq!(d("-7.5").checked_rem(d("2")).unwrap(), d("-1.5"));
        assert!(d("1").checked_rem(Decimal::ZERO).is_none());
        let big = d("99999999999999999999999999999999999999");
        assert!(big.checked_add(d("1")).is_none());
        assert!(big.checked_mul(d("10")).is_none());
    }

    #[test]
    fn ordering_equality_and_hash_are_numeric() {
        use std::collections::HashSet;
        assert_eq!(d("1.50"), d("1.5"));
        assert!(d("12345678901234567.89") > d("12345678901234567.88"));
        assert!(d("-0.5") < d("0.3"));
        assert!(d("-1.5") < d("-1.25"));
        assert!(d("1234567890123456789") > d("1234567890123456768"));
        assert!(d("0.00000000000000000000000000000000000001") > Decimal::ZERO);
        let set: HashSet<Decimal> = [d("2.0"), d("2"), d("2.000")].into_iter().collect();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn rounding_and_float_conversion() {
        assert_eq!(d("2.675").round(2).unwrap().to_string(), "2.68");
        assert_eq!(d("-2.5").round(0).unwrap().to_string(), "-3");
        assert_eq!(d("1250").round(-2).unwrap().to_string(), "1300");
        assert_eq!(d("-1.2").floor().unwrap().to_string(), "-2");
        assert_eq!(d("1.2").ceil().unwrap().to_string(), "2");
        assert_eq!(d("1.2").round(4).unwrap().to_string(), "1.2");
        assert!(d("0.5").fits_f64());
        assert!(!d("1234567890123456789").fits_f64());
        assert!(!d("12345678901234567.89").fits_f64());
        assert_eq!(Decimal::from_f64(0.1).unwrap().to_string(), "0.1");
        assert_eq!(Decimal::from_f64(-2.5e-3).unwrap().to_string(), "-0.0025");
    }
}
