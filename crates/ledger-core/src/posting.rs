use crate::{AccountAddress, AssetId};

/// Movement of an amount of one asset from a source account to a destination account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    source: AccountAddress,
    destination: AccountAddress,
    amount: u128,
    asset: AssetId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PostingError {
    #[error("posting amount must be greater than zero")]
    ZeroAmount,
    #[error("posting source and destination must differ")]
    SameAccount,
}

impl Posting {
    /// `amount` is in the asset's base units.
    pub fn new(
        source: AccountAddress,
        destination: AccountAddress,
        amount: u128,
        asset: AssetId,
    ) -> Result<Self, PostingError> {
        if amount == 0 {
            return Err(PostingError::ZeroAmount);
        }
        if source == destination {
            return Err(PostingError::SameAccount);
        }
        Ok(Self {
            source,
            destination,
            amount,
            asset,
        })
    }

    pub fn source(&self) -> &AccountAddress {
        &self.source
    }

    pub fn destination(&self) -> &AccountAddress {
        &self.destination
    }

    pub fn amount(&self) -> u128 {
        self.amount
    }

    pub fn asset(&self) -> &AssetId {
        &self.asset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(raw: &str) -> AccountAddress {
        raw.parse().unwrap()
    }

    fn usdc() -> AssetId {
        "USDC@ethereum/6".parse().unwrap()
    }

    #[test]
    fn creates_a_valid_posting() {
        let posting = Posting::new(
            account("users:1:wallet"),
            account("users:2:wallet"),
            1_500_000,
            usdc(),
        )
        .unwrap();
        assert_eq!(posting.source(), &account("users:1:wallet"));
        assert_eq!(posting.destination(), &account("users:2:wallet"));
        assert_eq!(posting.amount(), 1_500_000);
        assert_eq!(posting.asset(), &usdc());
    }

    #[test]
    fn accepts_the_full_u128_range() {
        assert!(
            Posting::new(
                AccountAddress::world(),
                account("users:1:wallet"),
                u128::MAX,
                usdc()
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_zero_amount() {
        let result = Posting::new(
            account("users:1:wallet"),
            account("users:2:wallet"),
            0,
            usdc(),
        );
        assert_eq!(result, Err(PostingError::ZeroAmount));
    }

    #[test]
    fn rejects_same_source_and_destination() {
        let result = Posting::new(
            account("users:1:wallet"),
            account("users:1:wallet"),
            1,
            usdc(),
        );
        assert_eq!(result, Err(PostingError::SameAccount));
    }
}
