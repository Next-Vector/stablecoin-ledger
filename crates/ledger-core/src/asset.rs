use std::fmt;
use std::str::FromStr;

/// A token on a specific network with its precision: `SYMBOL@network/decimals`, e.g. `USDC@ethereum/6`.
///
/// The same coin on different networks is a different asset.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetId {
    symbol: String,
    network: String,
    decimals: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssetIdError {
    #[error("asset id must have the form SYMBOL@network/decimals")]
    Format,
    #[error("asset symbol must be non-empty A-Z and 0-9")]
    InvalidSymbol,
    #[error("asset network must be non-empty a-z, 0-9 and _")]
    InvalidNetwork,
    #[error("asset decimals must be a non-negative integer")]
    InvalidDecimals,
    #[error("asset decimals {0} exceed the maximum of {max}", max = AssetId::MAX_DECIMALS)]
    DecimalsOutOfRange(u32),
}

impl AssetId {
    /// `u128` holds at most 38 full decimal digits.
    pub const MAX_DECIMALS: u8 = 38;

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn network(&self) -> &str {
        &self.network
    }

    pub fn decimals(&self) -> u8 {
        self.decimals
    }
}

impl FromStr for AssetId {
    type Err = AssetIdError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (symbol, rest) = raw.split_once('@').ok_or(AssetIdError::Format)?;
        let (network, decimals) = rest.split_once('/').ok_or(AssetIdError::Format)?;
        if symbol.is_empty() || network.is_empty() || decimals.is_empty() || network.contains('@') {
            return Err(AssetIdError::Format);
        }
        if !symbol.chars().all(|ch| matches!(ch, 'A'..='Z' | '0'..='9')) {
            return Err(AssetIdError::InvalidSymbol);
        }
        if !network
            .chars()
            .all(|ch| matches!(ch, 'a'..='z' | '0'..='9' | '_'))
        {
            return Err(AssetIdError::InvalidNetwork);
        }
        if !decimals.chars().all(|ch| ch.is_ascii_digit()) {
            return Err(AssetIdError::InvalidDecimals);
        }
        let decimals: u32 = decimals
            .parse()
            .map_err(|_| AssetIdError::InvalidDecimals)?;
        if decimals > u32::from(Self::MAX_DECIMALS) {
            return Err(AssetIdError::DecimalsOutOfRange(decimals));
        }
        Ok(Self {
            symbol: symbol.to_owned(),
            network: network.to_owned(),
            decimals: decimals as u8,
        })
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}/{}", self.symbol, self.network, self.decimals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_symbol_network_and_decimals() {
        let asset: AssetId = "USDC@ethereum/6".parse().unwrap();
        assert_eq!(asset.symbol(), "USDC");
        assert_eq!(asset.network(), "ethereum");
        assert_eq!(asset.decimals(), 6);
        assert_eq!(asset.to_string(), "USDC@ethereum/6");
    }

    #[test]
    fn round_trips_known_assets() {
        for raw in [
            "DAI@ethereum/18",
            "ETH@ethereum/18",
            "USDT@tron/6",
            "USDC@base_sepolia/6",
            "X@n/0",
            "BIG@n/38",
        ] {
            assert_eq!(raw.parse::<AssetId>().unwrap().to_string(), raw);
        }
    }

    #[test]
    fn same_coin_on_different_networks_is_a_different_asset() {
        let on_ethereum: AssetId = "USDC@ethereum/6".parse().unwrap();
        let on_base: AssetId = "USDC@base/6".parse().unwrap();
        assert_ne!(on_ethereum, on_base);
    }

    #[test]
    fn rejects_malformed_layout() {
        for raw in [
            "USDC",
            "USDC@ethereum",
            "USDC/6",
            "@ethereum/6",
            "USDC@/6",
            "USDC@ethereum/",
            "USDC@eth@x/6",
        ] {
            assert_eq!(raw.parse::<AssetId>(), Err(AssetIdError::Format), "{raw}");
        }
    }

    #[test]
    fn rejects_invalid_symbol() {
        for raw in ["usdc@ethereum/6", "US-DC@ethereum/6", "USDC.e@ethereum/6"] {
            assert_eq!(
                raw.parse::<AssetId>(),
                Err(AssetIdError::InvalidSymbol),
                "{raw}"
            );
        }
    }

    #[test]
    fn rejects_invalid_network() {
        for raw in ["USDC@Ethereum/6", "USDC@eth-main/6", "USDC@eth:main/6"] {
            assert_eq!(
                raw.parse::<AssetId>(),
                Err(AssetIdError::InvalidNetwork),
                "{raw}"
            );
        }
    }

    #[test]
    fn rejects_invalid_decimals() {
        for raw in [
            "USDC@ethereum/x",
            "USDC@ethereum/-1",
            "USDC@ethereum/+6",
            "USDC@ethereum/1.5",
        ] {
            assert_eq!(
                raw.parse::<AssetId>(),
                Err(AssetIdError::InvalidDecimals),
                "{raw}"
            );
        }
        assert_eq!(
            "USDC@ethereum/39".parse::<AssetId>(),
            Err(AssetIdError::DecimalsOutOfRange(39))
        );
    }
}
