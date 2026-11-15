use std::fmt;
use std::str::FromStr;

/// Hierarchical account address, e.g. `users:42:wallet`.
///
/// Segments of `[a-z0-9_]+` separated by `:`, at most [`AccountAddress::MAX_LEN`] characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountAddress(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountAddressError {
    #[error("account address is empty")]
    Empty,
    #[error("account address is {len} characters long, the maximum is {max}", max = AccountAddress::MAX_LEN)]
    TooLong { len: usize },
    #[error("account address has an empty segment")]
    EmptySegment,
    #[error(
        "account address contains invalid character {0:?}; allowed: a-z, 0-9, _ and : as separator"
    )]
    InvalidChar(char),
}

impl AccountAddress {
    pub const MAX_LEN: usize = 256;
    const WORLD: &'static str = "world";

    /// The system account representing the outside world.
    pub fn world() -> Self {
        Self(Self::WORLD.to_owned())
    }

    pub fn is_world(&self) -> bool {
        self.0 == Self::WORLD
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AccountAddress {
    type Err = AccountAddressError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.is_empty() {
            return Err(AccountAddressError::Empty);
        }
        if raw.len() > Self::MAX_LEN {
            return Err(AccountAddressError::TooLong { len: raw.len() });
        }
        for segment in raw.split(':') {
            if segment.is_empty() {
                return Err(AccountAddressError::EmptySegment);
            }
            if let Some(ch) = segment
                .chars()
                .find(|ch| !matches!(ch, 'a'..='z' | '0'..='9' | '_'))
            {
                return Err(AccountAddressError::InvalidChar(ch));
            }
        }
        Ok(Self(raw.to_owned()))
    }
}

impl fmt::Display for AccountAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_hierarchical_addresses() {
        for raw in [
            "world",
            "users:42:wallet",
            "platform:gas:ethereum",
            "chain:ethereum:in_flight",
            "holds:01_ab",
        ] {
            let address: AccountAddress = raw.parse().unwrap();
            assert_eq!(address.as_str(), raw);
            assert_eq!(address.to_string(), raw);
        }
    }

    #[test]
    fn accepts_maximum_length() {
        let raw = "a".repeat(AccountAddress::MAX_LEN);
        assert!(raw.parse::<AccountAddress>().is_ok());
    }

    #[test]
    fn rejects_empty_address() {
        assert_eq!(
            "".parse::<AccountAddress>(),
            Err(AccountAddressError::Empty)
        );
    }

    #[test]
    fn rejects_too_long_address() {
        let raw = "a".repeat(AccountAddress::MAX_LEN + 1);
        assert_eq!(
            raw.parse::<AccountAddress>(),
            Err(AccountAddressError::TooLong {
                len: AccountAddress::MAX_LEN + 1
            })
        );
    }

    #[test]
    fn rejects_empty_segments() {
        for raw in [":users", "users:", "users::wallet"] {
            assert_eq!(
                raw.parse::<AccountAddress>(),
                Err(AccountAddressError::EmptySegment),
                "{raw}"
            );
        }
    }

    #[test]
    fn rejects_characters_outside_the_allowed_set() {
        for (raw, ch) in [
            ("Users:1", 'U'),
            ("users:1:wal-let", '-'),
            ("users 1", ' '),
            ("users:*:wallet", '*'),
            ("usérs", 'é'),
        ] {
            assert_eq!(
                raw.parse::<AccountAddress>(),
                Err(AccountAddressError::InvalidChar(ch)),
                "{raw}"
            );
        }
    }

    #[test]
    fn world_is_the_world_account() {
        assert!(AccountAddress::world().is_world());
        assert_eq!(AccountAddress::world().as_str(), "world");
        assert!(
            !"users:1:wallet"
                .parse::<AccountAddress>()
                .unwrap()
                .is_world()
        );
        assert!(!"world:x".parse::<AccountAddress>().unwrap().is_world());
    }
}
