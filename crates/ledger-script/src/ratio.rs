//! Exact portions of a whole and the deterministic split of an amount into shares (FR-63).

use std::fmt;

/// A portion `numerator/denominator` in lowest terms, between 0 and 1. Denominators are capped
/// so `amount × portion` is computed exactly in `u128`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio {
    numerator: u128,
    denominator: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RatioError {
    #[error("a portion cannot divide by zero")]
    ZeroDenominator,
    #[error("a portion cannot exceed 100%")]
    AboveWhole,
    #[error("a portion is too precise; its denominator must be at most 10^18")]
    TooPrecise,
    #[error("a portion must look like `12.5%` or `1/3`")]
    Format,
}

impl Ratio {
    /// Denominators up to 10^18 keep every intermediate product below `u128::MAX`.
    pub const MAX_DENOMINATOR: u128 = 1_000_000_000_000_000_000;

    pub fn new(numerator: u128, denominator: u128) -> Result<Self, RatioError> {
        if denominator == 0 {
            return Err(RatioError::ZeroDenominator);
        }
        if numerator > denominator {
            return Err(RatioError::AboveWhole);
        }
        let divisor = gcd(numerator, denominator);
        let ratio = Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        };
        if ratio.denominator > Self::MAX_DENOMINATOR {
            return Err(RatioError::TooPrecise);
        }
        Ok(ratio)
    }

    /// `12.5` read as a percentage.
    pub fn percent(digits: &str) -> Result<Self, RatioError> {
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        if whole.is_empty() || !is_digits(whole) || (digits.contains('.') && !is_digits(fraction)) {
            return Err(RatioError::Format);
        }
        let exponent = u32::try_from(fraction.len()).map_err(|_| RatioError::TooPrecise)?;
        let scale = 10u128
            .checked_pow(exponent)
            .and_then(|scale| scale.checked_mul(100))
            .ok_or(RatioError::TooPrecise)?;
        let numerator = format!("{whole}{fraction}")
            .parse::<u128>()
            .map_err(|_| RatioError::AboveWhole)?;
        Self::new(numerator, scale)
    }

    /// `12.5%` or `1/3`.
    pub fn parse(raw: &str) -> Result<Self, RatioError> {
        if let Some(percent) = raw.strip_suffix('%') {
            return Self::percent(percent);
        }
        let (numerator, denominator) = raw.split_once('/').ok_or(RatioError::Format)?;
        if !is_digits(numerator) || !is_digits(denominator) {
            return Err(RatioError::Format);
        }
        let parse = |digits: &str| digits.parse::<u128>().map_err(|_| RatioError::TooPrecise);
        Self::new(parse(numerator)?, parse(denominator)?)
    }

    pub fn numerator(self) -> u128 {
        self.numerator
    }

    pub fn denominator(self) -> u128 {
        self.denominator
    }

    /// `⌊amount × self⌋`, exactly: with `amount = q·d + r`, it is `q·n + ⌊r·n / d⌋`, where `q·n ≤
    /// amount` and `r·n < d² ≤ 10^36` cannot overflow.
    pub fn floor_of(self, amount: u128) -> u128 {
        let (quotient, remainder) = (amount / self.denominator, amount % self.denominator);
        quotient * self.numerator + remainder * self.numerator / self.denominator
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.numerator, self.denominator)
    }
}

/// The sum of portions as a fraction in lowest terms (it may exceed 1); `None` if the common
/// denominator overflows.
pub fn sum(portions: &[Ratio]) -> Option<(u128, u128)> {
    portions.iter().try_fold((0u128, 1u128), |(n, d), portion| {
        let common = d.checked_mul(portion.denominator / gcd(d, portion.denominator))?;
        let numerator = n.checked_mul(common / d)?.checked_add(
            portion
                .numerator
                .checked_mul(common / portion.denominator)?,
        )?;
        let divisor = gcd(numerator, common);
        Some((numerator / divisor, common / divisor))
    })
}

/// How the shares of a split are defined: portions, and optionally one share taking the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Share {
    Portion(Ratio),
    Remaining,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SplitError {
    #[error("portions sum to {numerator}/{denominator}; without `remaining` they must sum to 100%")]
    NotWhole { numerator: u128, denominator: u128 },
    #[error("portions sum to {numerator}/{denominator}, more than 100%")]
    AboveWhole { numerator: u128, denominator: u128 },
    #[error("portions are too fine-grained to add up exactly")]
    TooFine,
}

/// Splits `total` into the shares, which always sum to exactly `total`. Each portion gets
/// `⌊total × portion⌋`; `remaining` gets what is left, and without it the leftover base units
/// (fewer than the number of non-zero portions) go one each to the non-zero portions in order.
pub fn split(total: u128, shares: &[Share]) -> Result<Vec<u128>, SplitError> {
    let portions: Vec<Ratio> = shares
        .iter()
        .filter_map(|share| match share {
            Share::Portion(ratio) => Some(*ratio),
            Share::Remaining => None,
        })
        .collect();
    let (numerator, denominator) = sum(&portions).ok_or(SplitError::TooFine)?;
    let has_remaining = shares.contains(&Share::Remaining);
    if numerator > denominator {
        return Err(SplitError::AboveWhole {
            numerator,
            denominator,
        });
    }
    if !has_remaining && numerator != denominator {
        return Err(SplitError::NotWhole {
            numerator,
            denominator,
        });
    }

    let mut amounts: Vec<u128> = shares
        .iter()
        .map(|share| match share {
            Share::Portion(ratio) => ratio.floor_of(total),
            Share::Remaining => 0,
        })
        .collect();
    // Portions sum to at most 1, so their floors sum to at most `total`.
    let mut leftover = total - amounts.iter().sum::<u128>();
    for (amount, share) in amounts.iter_mut().zip(shares) {
        match share {
            Share::Remaining => {
                *amount = leftover;
                leftover = 0;
            }
            Share::Portion(ratio) if !has_remaining && leftover > 0 && ratio.numerator > 0 => {
                *amount += 1;
                leftover -= 1;
            }
            Share::Portion(_) => {}
        }
    }
    Ok(amounts)
}

fn is_digits(raw: &str) -> bool {
    !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit())
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn ratio(numerator: u128, denominator: u128) -> Ratio {
        Ratio::new(numerator, denominator).unwrap()
    }

    #[test]
    fn parses_percentages_and_fractions_in_lowest_terms() {
        assert_eq!(Ratio::parse("12.5%").unwrap(), ratio(1, 8));
        assert_eq!(Ratio::parse("100%").unwrap(), ratio(1, 1));
        assert_eq!(Ratio::parse("0%").unwrap(), ratio(0, 1));
        assert_eq!(Ratio::parse("50/100").unwrap(), ratio(1, 2));
        assert_eq!(Ratio::parse("101%"), Err(RatioError::AboveWhole));
        assert_eq!(Ratio::parse("1/0"), Err(RatioError::ZeroDenominator));
        assert_eq!(
            Ratio::parse("1/1000000000000000001"),
            Err(RatioError::TooPrecise)
        );
        for bad in [
            "", "%", "1", "1.%", ".5%", "1/", "/2", "-1/2", "a/b", "1.5/2",
        ] {
            assert_eq!(Ratio::parse(bad), Err(RatioError::Format), "{bad:?}");
        }
    }

    #[test]
    fn floors_exactly_even_for_the_largest_amounts() {
        assert_eq!(ratio(1, 3).floor_of(100), 33);
        assert_eq!(ratio(1, 1).floor_of(u128::MAX), u128::MAX);
        assert_eq!(
            ratio(999_999_999_999_999_999, Ratio::MAX_DENOMINATOR).floor_of(u128::MAX),
            u128::MAX - u128::MAX / Ratio::MAX_DENOMINATOR - 1
        );
    }

    #[test]
    fn leftover_units_go_to_portions_in_order() {
        let third = Share::Portion(ratio(1, 3));
        assert_eq!(split(100, &[third, third, third]).unwrap(), [34, 33, 33]);
        assert_eq!(split(2, &[third, third, third]).unwrap(), [1, 1, 0]);
        let zero = Share::Portion(ratio(0, 1));
        let half = Share::Portion(ratio(1, 2));
        assert_eq!(split(3, &[zero, half, half]).unwrap(), [0, 2, 1]);
        assert_eq!(split(10, &[third, Share::Remaining]).unwrap(), [3, 7]);
    }

    #[test]
    fn portions_too_fine_to_add_exactly_are_an_error_not_a_wrong_split() {
        let primes = [
            1_000_003, 1_000_033, 1_000_037, 1_000_039, 1_000_081, 1_000_099, 1_000_117,
        ];
        let shares: Vec<Share> = primes
            .iter()
            .map(|&p| Share::Portion(ratio(1, p)))
            .chain([Share::Remaining])
            .collect();
        assert_eq!(split(100, &shares), Err(SplitError::TooFine));
    }

    #[test]
    fn rejects_portions_that_do_not_add_up() {
        let tenth = Share::Portion(ratio(1, 10));
        assert_eq!(
            split(10, &[tenth, tenth]),
            Err(SplitError::NotWhole {
                numerator: 1,
                denominator: 5
            })
        );
        let most = Share::Portion(ratio(9, 10));
        assert_eq!(
            split(10, &[most, tenth, tenth, Share::Remaining]),
            Err(SplitError::AboveWhole {
                numerator: 11,
                denominator: 10
            })
        );
    }

    /// Up to 8 portions with denominators ≤ 10^4, so their common denominator almost always
    /// fits; the rare overflow (`TooFine`, tested separately) is skipped with `prop_assume!`.
    fn portions() -> impl Strategy<Value = Vec<Ratio>> {
        prop::collection::vec((1u128..=10_000, 0u128..=10_000), 1..=8).prop_map(|raw| {
            raw.into_iter()
                .map(|(d, n)| ratio(n % (d + 1), d))
                .collect()
        })
    }

    proptest! {
        /// FR-63: whatever the portions, the shares sum to exactly the input.
        #[test]
        fn shares_always_sum_to_the_input(total: u128, portions in portions()) {
            // Scale the portions down so they fit under 100%, then let `remaining` take the rest.
            let count = portions.len() as u128;
            let scaled: Vec<Ratio> = portions
                .iter()
                .map(|p| ratio(p.numerator(), p.denominator() * count))
                .collect();
            prop_assume!(sum(&scaled).is_some());
            let shares: Vec<Share> = scaled
                .into_iter()
                .map(Share::Portion)
                .chain([Share::Remaining])
                .collect();
            let amounts = split(total, &shares).unwrap();
            prop_assert_eq!(amounts.iter().sum::<u128>(), total);
        }

        /// Without `remaining`: portions of one denominator that sum to exactly 100%.
        #[test]
        fn shares_without_remaining_sum_to_the_input(
            total: u128,
            denominator in 1u128..=Ratio::MAX_DENOMINATOR,
            cuts in prop::collection::vec(any::<u64>(), 0..10),
        ) {
            // Sorted cut points in [0, denominator] give numerators that sum to the denominator.
            let mut points: Vec<u128> = cuts.iter().map(|&c| u128::from(c) % (denominator + 1)).collect();
            points.push(0);
            points.push(denominator);
            points.sort_unstable();
            let shares: Vec<Share> = points
                .windows(2)
                .map(|pair| Share::Portion(ratio(pair[1] - pair[0], denominator)))
                .collect();
            let amounts = split(total, &shares).unwrap();
            prop_assert_eq!(amounts.iter().sum::<u128>(), total);
            for (amount, share) in amounts.iter().zip(&shares) {
                let Share::Portion(r) = share else { unreachable!() };
                let exact = r.floor_of(total);
                prop_assert!(*amount == exact || *amount == exact + 1);
            }
        }
    }
}
