use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// Signed account balance in base units, covering ±`u128::MAX`.
///
/// Sign-magnitude, because posting amounts span the full `u128` range and `i128` cannot hold it.
/// All arithmetic is checked: overflow is an error, never a panic or a wrap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Balance {
    negative: bool,
    magnitude: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BalanceError {
    #[error("balance overflow")]
    Overflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("balance must be an optionally negative integer within ±u128::MAX")]
pub struct ParseBalanceError;

impl Balance {
    pub const ZERO: Self = Self {
        negative: false,
        magnitude: 0,
    };

    pub fn is_zero(&self) -> bool {
        self.magnitude == 0
    }

    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// Adds `amount` (money arriving at the account).
    pub fn credit(self, amount: u128) -> Result<Self, BalanceError> {
        self.shift(amount, false)
    }

    /// Subtracts `amount` (money leaving the account).
    pub fn debit(self, amount: u128) -> Result<Self, BalanceError> {
        self.shift(amount, true)
    }

    /// Adds two signed balances.
    pub fn checked_add(self, other: Self) -> Result<Self, BalanceError> {
        self.shift(other.magnitude, other.negative)
    }

    /// Moves the balance by `amount` towards negative (`down`) or positive.
    fn shift(self, amount: u128, down: bool) -> Result<Self, BalanceError> {
        if self.is_zero() || self.negative == down {
            let magnitude = self
                .magnitude
                .checked_add(amount)
                .ok_or(BalanceError::Overflow)?;
            return Ok(Self::signed(down, magnitude));
        }
        Ok(match self.magnitude.checked_sub(amount) {
            Some(rest) => Self::signed(self.negative, rest),
            None => Self::signed(down, amount - self.magnitude),
        })
    }

    /// Builds a balance keeping zero non-negative, so equality stays structural.
    fn signed(negative: bool, magnitude: u128) -> Self {
        Self {
            negative: negative && magnitude != 0,
            magnitude,
        }
    }
}

impl Ord for Balance {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, false) => self.magnitude.cmp(&other.magnitude),
            (true, true) => other.magnitude.cmp(&self.magnitude),
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
        }
    }
}

impl PartialOrd for Balance {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl FromStr for Balance {
    type Err = ParseBalanceError;

    /// Parses the [`Display`](fmt::Display) form: digits with an optional leading `-`.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (negative, digits) = match raw.strip_prefix('-') {
            Some(digits) => (true, digits),
            None => (false, raw),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseBalanceError);
        }
        let magnitude: u128 = digits.parse().map_err(|_| ParseBalanceError)?;
        Ok(Self::signed(negative, magnitude))
    }
}

impl fmt::Display for Balance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.negative {
            f.write_str("-")?;
        }
        write!(f, "{}", self.magnitude)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_zero() {
        assert_eq!(Balance::ZERO, Balance::default());
        assert!(Balance::ZERO.is_zero());
        assert!(!Balance::ZERO.is_negative());
    }

    #[test]
    fn credits_and_debits_across_zero() {
        let balance = Balance::ZERO.credit(10).unwrap().debit(25).unwrap();
        assert!(balance.is_negative());
        assert_eq!(balance.to_string(), "-15");

        let balance = balance.credit(15).unwrap();
        assert!(balance.is_zero());
        assert!(!balance.is_negative());
        assert_eq!(balance.to_string(), "0");
    }

    #[test]
    fn represents_the_full_u128_range_in_both_directions() {
        let positive = Balance::ZERO.credit(u128::MAX).unwrap();
        assert_eq!(positive.to_string(), u128::MAX.to_string());
        let negative = Balance::ZERO.debit(u128::MAX).unwrap();
        assert_eq!(negative.to_string(), format!("-{}", u128::MAX));
        assert!(
            negative
                .credit(u128::MAX)
                .unwrap()
                .credit(u128::MAX)
                .is_ok()
        );
    }

    #[test]
    fn overflow_is_an_error_not_a_panic() {
        let max = Balance::ZERO.credit(u128::MAX).unwrap();
        assert_eq!(max.credit(1), Err(BalanceError::Overflow));
        let min = Balance::ZERO.debit(u128::MAX).unwrap();
        assert_eq!(min.debit(1), Err(BalanceError::Overflow));
    }

    #[test]
    fn adds_signed_balances_with_overflow_check() {
        let five = Balance::ZERO.credit(5).unwrap();
        let minus_eight = Balance::ZERO.debit(8).unwrap();
        assert_eq!(
            five.checked_add(minus_eight),
            Ok(Balance::ZERO.debit(3).unwrap())
        );
        assert_eq!(
            minus_eight.checked_add(minus_eight),
            Ok(Balance::ZERO.debit(16).unwrap())
        );
        let max = Balance::ZERO.credit(u128::MAX).unwrap();
        assert_eq!(max.checked_add(five), Err(BalanceError::Overflow));
        assert_eq!(
            max.checked_add(minus_eight),
            Ok(Balance::ZERO.credit(u128::MAX - 8).unwrap())
        );
    }

    #[test]
    fn parses_its_own_display_form() {
        for value in [
            "0",
            "15",
            "-15",
            &u128::MAX.to_string(),
            &format!("-{}", u128::MAX),
        ] {
            assert_eq!(value.parse::<Balance>().unwrap().to_string(), value);
        }
        assert_eq!("-0".parse::<Balance>(), Ok(Balance::ZERO));
    }

    #[test]
    fn rejects_malformed_balances() {
        for value in [
            "",
            "-",
            "+5",
            "1.5",
            "abc",
            " 5",
            &format!("{}0", u128::MAX),
        ] {
            assert_eq!(
                value.parse::<Balance>(),
                Err(ParseBalanceError),
                "{value:?}"
            );
        }
    }

    #[test]
    fn orders_by_signed_value() {
        let minus_five = Balance::ZERO.debit(5).unwrap();
        let minus_one = Balance::ZERO.debit(1).unwrap();
        let three = Balance::ZERO.credit(3).unwrap();
        assert!(minus_five < minus_one);
        assert!(minus_one < Balance::ZERO);
        assert!(Balance::ZERO < three);
    }
}
