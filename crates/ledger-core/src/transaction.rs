use std::collections::BTreeMap;

use crate::Posting;

/// Arbitrary string key–value pairs attached to a transaction.
pub type Metadata = BTreeMap<String, String>;

/// An atomic set of postings: applied in full or not at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    postings: Vec<Posting>,
    metadata: Metadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransactionError {
    #[error("transaction must contain at least one posting")]
    NoPostings,
    #[error("transaction has {0} postings, the maximum is {max}", max = Transaction::MAX_POSTINGS)]
    TooManyPostings(usize),
}

impl Transaction {
    pub const MAX_POSTINGS: usize = 256;

    pub fn new(postings: Vec<Posting>, metadata: Metadata) -> Result<Self, TransactionError> {
        if postings.is_empty() {
            return Err(TransactionError::NoPostings);
        }
        if postings.len() > Self::MAX_POSTINGS {
            return Err(TransactionError::TooManyPostings(postings.len()));
        }
        Ok(Self { postings, metadata })
    }

    pub fn postings(&self) -> &[Posting] {
        &self.postings
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AccountAddress;

    fn posting(amount: u128) -> Posting {
        Posting::new(
            AccountAddress::world(),
            "users:1:wallet".parse().unwrap(),
            amount,
            "USDC@ethereum/6".parse().unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn creates_a_transaction_with_postings_and_metadata() {
        let metadata = Metadata::from([("order_id".to_owned(), "8812".to_owned())]);
        let tx = Transaction::new(vec![posting(1), posting(2)], metadata.clone()).unwrap();
        assert_eq!(tx.postings(), &[posting(1), posting(2)]);
        assert_eq!(tx.metadata(), &metadata);
    }

    #[test]
    fn accepts_the_maximum_number_of_postings() {
        let postings = vec![posting(1); Transaction::MAX_POSTINGS];
        assert!(Transaction::new(postings, Metadata::new()).is_ok());
    }

    #[test]
    fn rejects_a_transaction_without_postings() {
        assert_eq!(
            Transaction::new(vec![], Metadata::new()),
            Err(TransactionError::NoPostings)
        );
    }

    #[test]
    fn rejects_too_many_postings() {
        let postings = vec![posting(1); Transaction::MAX_POSTINGS + 1];
        assert_eq!(
            Transaction::new(postings, Metadata::new()),
            Err(TransactionError::TooManyPostings(
                Transaction::MAX_POSTINGS + 1
            ))
        );
    }
}
