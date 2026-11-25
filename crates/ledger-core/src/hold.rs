//! Holds: reserve funds, then capture (fully or partially) or void them. Every hold action is an
//! ordinary journal transaction through the technical account `holds:{id}`, so the zero-sum
//! invariant holds. Time is passed in explicitly (Unix seconds) to keep the core deterministic.
//!
//! The rules live on [`Hold`] as pure functions that build the transaction for each action, so
//! the in-memory [`Journal`] and the PostgreSQL store apply exactly the same logic.

use crate::{
    AccountAddress, ApplyError, AssetId, Journal, Metadata, Posting, PostingError, Transaction,
    TxId,
};

/// Hold id assigned by the store, starting at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HoldId(pub u64);

impl HoldId {
    /// The technical account `holds:{id}` that holds the reserved funds.
    pub fn account(self) -> AccountAddress {
        format!("holds:{}", self.0)
            .parse()
            .expect("holds:{id} is a valid account address")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldStatus {
    Active,
    Captured,
    Voided,
    Expired,
}

impl HoldStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HoldStatus::Active => "active",
            HoldStatus::Captured => "captured",
            HoldStatus::Voided => "voided",
            HoldStatus::Expired => "expired",
        }
    }
}

impl std::str::FromStr for HoldStatus {
    type Err = UnknownHoldStatus;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "active" => Ok(HoldStatus::Active),
            "captured" => Ok(HoldStatus::Captured),
            "voided" => Ok(HoldStatus::Voided),
            "expired" => Ok(HoldStatus::Expired),
            other => Err(UnknownHoldStatus(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown hold status {0:?}")]
pub struct UnknownHoldStatus(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    id: HoldId,
    owner: AccountAddress,
    asset: AssetId,
    amount: u128,
    status: HoldStatus,
    expires_at: Option<u64>,
}

impl Hold {
    /// Destinations of one capture; with the owner's remainder a transaction holds 256 postings.
    pub const MAX_CAPTURE_DESTINATIONS: usize = 255;

    /// A hold of `amount` (> 0) of `asset` reserved from `owner`, in the given state.
    pub fn new(
        id: HoldId,
        owner: AccountAddress,
        amount: u128,
        asset: AssetId,
        status: HoldStatus,
        expires_at: Option<u64>,
    ) -> Result<Self, HoldError> {
        if amount == 0 {
            return Err(HoldError::ZeroAmount);
        }
        Ok(Self {
            id,
            owner,
            asset,
            amount,
            status,
            expires_at,
        })
    }

    pub fn id(&self) -> HoldId {
        self.id
    }

    /// The account the funds were reserved from and return to.
    pub fn owner(&self) -> &AccountAddress {
        &self.owner
    }

    /// The technical account `holds:{id}` holding the reserved funds.
    pub fn account(&self) -> AccountAddress {
        self.id.account()
    }

    pub fn asset(&self) -> &AssetId {
        &self.asset
    }

    pub fn amount(&self) -> u128 {
        self.amount
    }

    pub fn status(&self) -> HoldStatus {
        self.status
    }

    /// Unix seconds after which the hold expires, if any.
    pub fn expires_at(&self) -> Option<u64> {
        self.expires_at
    }

    /// An active hold whose deadline is at or before `now` (Unix seconds): it must be voided as
    /// expired and can no longer be captured.
    pub fn is_due(&self, now: u64) -> bool {
        self.status == HoldStatus::Active && self.expires_at.is_some_and(|at| at <= now)
    }

    /// The transaction that reserves the funds: owner → `holds:{id}`.
    pub fn reservation(&self, metadata: Metadata) -> Result<Transaction, HoldError> {
        let posting = Posting::new(
            self.owner.clone(),
            self.account(),
            self.amount,
            self.asset.clone(),
        )?;
        Ok(single(posting, metadata))
    }

    /// The transaction that sends `amount` (≤ the held amount) to `destination` and returns the
    /// rest to the owner. Only an active hold can be captured.
    pub fn capture(
        &self,
        destination: AccountAddress,
        amount: u128,
        metadata: Metadata,
    ) -> Result<Transaction, HoldError> {
        self.capture_to(&[(destination, amount)], metadata)
    }

    /// The transaction that sends each amount to its destination (together ≤ the held amount)
    /// and returns the rest to the owner, e.g. a withdrawal's amount in flight and its fee.
    pub fn capture_to(
        &self,
        destinations: &[(AccountAddress, u128)],
        metadata: Metadata,
    ) -> Result<Transaction, HoldError> {
        self.ensure_active()?;
        if destinations.is_empty() {
            return Err(HoldError::NoDestinations);
        }
        if destinations.len() > Self::MAX_CAPTURE_DESTINATIONS {
            return Err(HoldError::TooManyDestinations(destinations.len()));
        }
        if destinations.iter().any(|(_, amount)| *amount == 0) {
            return Err(HoldError::ZeroAmount);
        }
        let exceeds = |requested| HoldError::ExceedsHold {
            requested,
            held: self.amount,
        };
        let captured = destinations
            .iter()
            .try_fold(0u128, |sum, (_, amount)| sum.checked_add(*amount))
            .ok_or_else(|| exceeds(u128::MAX))?;
        if captured > self.amount {
            return Err(exceeds(captured));
        }
        let mut postings = destinations
            .iter()
            .map(|(destination, amount)| {
                Posting::new(
                    self.account(),
                    destination.clone(),
                    *amount,
                    self.asset.clone(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        if captured < self.amount {
            postings.push(Posting::new(
                self.account(),
                self.owner.clone(),
                self.amount - captured,
                self.asset.clone(),
            )?);
        }
        Ok(Transaction::new(postings, metadata).expect("1 to 256 postings"))
    }

    /// The transaction that returns the whole held amount to the owner (void or expiry).
    pub fn release(&self, metadata: Metadata) -> Result<Transaction, HoldError> {
        self.ensure_active()?;
        let posting = Posting::new(
            self.account(),
            self.owner.clone(),
            self.amount,
            self.asset.clone(),
        )?;
        Ok(single(posting, metadata))
    }

    fn ensure_active(&self) -> Result<(), HoldError> {
        match self.status {
            HoldStatus::Active => Ok(()),
            status => Err(HoldError::NotActive(self.id, status)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HoldError {
    #[error("hold {0:?} does not exist")]
    NotFound(HoldId),
    #[error("hold {0:?} is {1:?}, not active")]
    NotActive(HoldId, HoldStatus),
    #[error("hold amount must be greater than zero")]
    ZeroAmount,
    #[error("a capture needs at least one destination")]
    NoDestinations,
    #[error("a capture has {0} destinations; at most {max} are allowed", max = Hold::MAX_CAPTURE_DESTINATIONS)]
    TooManyDestinations(usize),
    #[error("capture of {requested} exceeds the held {held}")]
    ExceedsHold { requested: u128, held: u128 },
    #[error("invalid posting: {0}")]
    Posting(#[from] PostingError),
    #[error(transparent)]
    Apply(#[from] ApplyError),
}

impl Journal {
    /// Reserves `amount` of `asset` from `owner` until captured, voided or expired.
    pub fn hold(
        &mut self,
        owner: AccountAddress,
        amount: u128,
        asset: AssetId,
        expires_at: Option<u64>,
        metadata: Metadata,
    ) -> Result<HoldId, HoldError> {
        let id = HoldId(self.holds.len() as u64 + 1);
        let hold = Hold::new(id, owner, amount, asset, HoldStatus::Active, expires_at)?;
        self.apply(hold.reservation(metadata)?)?;
        self.holds.push(hold);
        Ok(id)
    }

    /// Sends `amount` (≤ the held amount) to `destination` and returns the rest to the owner,
    /// in one transaction. A hold is captured at most once.
    pub fn capture(
        &mut self,
        id: HoldId,
        destination: AccountAddress,
        amount: u128,
        metadata: Metadata,
    ) -> Result<TxId, HoldError> {
        self.capture_to(id, &[(destination, amount)], metadata)
    }

    /// Sends each amount to its destination (together ≤ the held amount) and returns the rest to
    /// the owner, in one transaction.
    pub fn capture_to(
        &mut self,
        id: HoldId,
        destinations: &[(AccountAddress, u128)],
        metadata: Metadata,
    ) -> Result<TxId, HoldError> {
        let tx = self.find_hold(id)?.capture_to(destinations, metadata)?;
        let tx_id = self.apply(tx)?;
        self.set_status(id, HoldStatus::Captured);
        Ok(tx_id)
    }

    /// Returns the whole held amount to the owner.
    pub fn void(&mut self, id: HoldId, metadata: Metadata) -> Result<TxId, HoldError> {
        self.release(id, HoldStatus::Voided, metadata)
    }

    /// Voids every active hold whose deadline is at or before `now` (Unix seconds) and returns
    /// their ids. Each expiry is its own transaction; on error, earlier expiries stay applied.
    pub fn expire(&mut self, now: u64) -> Result<Vec<HoldId>, HoldError> {
        let due: Vec<HoldId> = self
            .holds
            .iter()
            .filter(|hold| hold.is_due(now))
            .map(Hold::id)
            .collect();
        for &id in &due {
            self.release(id, HoldStatus::Expired, Metadata::new())?;
        }
        Ok(due)
    }

    pub fn get_hold(&self, id: HoldId) -> Option<&Hold> {
        self.holds.get(Self::hold_index(id)?)
    }

    fn release(
        &mut self,
        id: HoldId,
        status: HoldStatus,
        metadata: Metadata,
    ) -> Result<TxId, HoldError> {
        let tx = self.find_hold(id)?.release(metadata)?;
        let tx_id = self.apply(tx)?;
        self.set_status(id, status);
        Ok(tx_id)
    }

    fn find_hold(&self, id: HoldId) -> Result<&Hold, HoldError> {
        self.get_hold(id).ok_or(HoldError::NotFound(id))
    }

    fn set_status(&mut self, id: HoldId, status: HoldStatus) {
        if let Some(hold) = Self::hold_index(id).and_then(|index| self.holds.get_mut(index)) {
            hold.status = status;
        }
    }

    fn hold_index(id: HoldId) -> Option<usize> {
        usize::try_from(id.0).ok()?.checked_sub(1)
    }
}

fn single(posting: Posting, metadata: Metadata) -> Transaction {
    Transaction::new(vec![posting], metadata).expect("a single posting is a valid transaction")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Balance;

    fn account(raw: &str) -> AccountAddress {
        raw.parse().unwrap()
    }

    fn usdc() -> AssetId {
        "USDC@ethereum/6".parse().unwrap()
    }

    fn amount(value: u128) -> Balance {
        Balance::ZERO.credit(value).unwrap()
    }

    /// Journal with users:1:wallet funded with 100 USDC.
    fn funded() -> Journal {
        let mut journal = Journal::default();
        let funding = Posting::new(
            AccountAddress::world(),
            account("users:1:wallet"),
            100,
            usdc(),
        )
        .unwrap();
        journal
            .apply(Transaction::new(vec![funding], Metadata::new()).unwrap())
            .unwrap();
        journal
    }

    fn balance(journal: &Journal, raw: &str) -> Balance {
        journal.balance(&account(raw), &usdc())
    }

    #[test]
    fn hold_moves_funds_to_the_hold_account() {
        let mut journal = funded();
        let id = journal
            .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
            .unwrap();

        let hold = journal.get_hold(id).unwrap();
        assert_eq!(hold.owner(), &account("users:1:wallet"));
        assert_eq!(hold.amount(), 30);
        assert_eq!(hold.status(), HoldStatus::Active);
        assert_eq!(balance(&journal, "users:1:wallet"), amount(70));
        assert_eq!(journal.balance(&hold.account(), &usdc()), amount(30));
        assert_eq!(hold.account().as_str(), format!("holds:{}", id.0));
    }

    #[test]
    fn hold_follows_the_usual_rules() {
        let mut journal = funded();
        assert_eq!(
            journal.hold(
                account("users:1:wallet"),
                101,
                usdc(),
                None,
                Metadata::new()
            ),
            Err(HoldError::Apply(ApplyError::InsufficientFunds {
                account: account("users:1:wallet"),
                asset: usdc()
            }))
        );
        assert_eq!(
            journal.hold(account("users:1:wallet"), 0, usdc(), None, Metadata::new()),
            Err(HoldError::ZeroAmount)
        );
    }

    #[test]
    fn full_capture_spends_the_whole_hold() {
        let mut journal = funded();
        let id = journal
            .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
            .unwrap();
        journal
            .capture(id, account("merchants:7:wallet"), 30, Metadata::new())
            .unwrap();

        assert_eq!(journal.get_hold(id).unwrap().status(), HoldStatus::Captured);
        assert_eq!(balance(&journal, "merchants:7:wallet"), amount(30));
        assert_eq!(balance(&journal, "users:1:wallet"), amount(70));
        assert_eq!(
            journal.balance(&journal.get_hold(id).unwrap().account(), &usdc()),
            Balance::ZERO
        );
    }

    #[test]
    fn partial_capture_returns_the_remainder_to_the_owner_in_one_transaction() {
        let mut journal = funded();
        let id = journal
            .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
            .unwrap();
        let tx = journal
            .capture(id, account("merchants:7:wallet"), 20, Metadata::new())
            .unwrap();

        assert_eq!(journal.transaction(tx).unwrap().postings().len(), 2);
        assert_eq!(balance(&journal, "merchants:7:wallet"), amount(20));
        assert_eq!(balance(&journal, "users:1:wallet"), amount(80));
        assert_eq!(
            journal.balance(&journal.get_hold(id).unwrap().account(), &usdc()),
            Balance::ZERO
        );
    }

    #[test]
    fn capture_cannot_exceed_the_hold() {
        let mut journal = funded();
        let id = journal
            .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
            .unwrap();
        assert_eq!(
            journal.capture(id, account("merchants:7:wallet"), 31, Metadata::new()),
            Err(HoldError::ExceedsHold {
                requested: 31,
                held: 30
            })
        );
        assert_eq!(
            journal.capture(id, account("merchants:7:wallet"), 0, Metadata::new()),
            Err(HoldError::ZeroAmount)
        );
        assert_eq!(journal.get_hold(id).unwrap().status(), HoldStatus::Active);
    }

    #[test]
    fn a_capture_splits_between_destinations_in_one_transaction() {
        // A withdrawal: the amount goes in flight, the fee to the platform, the rest back.
        let mut journal = funded();
        let id = journal
            .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
            .unwrap();
        let tx = journal
            .capture_to(
                id,
                &[
                    (account("chain:ethereum:in_flight"), 25),
                    (account("platform:fees"), 1),
                ],
                Metadata::new(),
            )
            .unwrap();

        let postings = journal.transaction(tx).unwrap().postings();
        let moved: Vec<(String, u128)> = postings
            .iter()
            .map(|p| (p.destination().to_string(), p.amount()))
            .collect();
        assert_eq!(
            moved,
            [
                ("chain:ethereum:in_flight".to_owned(), 25),
                ("platform:fees".to_owned(), 1),
                ("users:1:wallet".to_owned(), 4),
            ]
        );
        assert_eq!(journal.get_hold(id).unwrap().status(), HoldStatus::Captured);
        assert_eq!(balance(&journal, "users:1:wallet"), amount(74));
        assert_eq!(
            journal.balance(&journal.get_hold(id).unwrap().account(), &usdc()),
            Balance::ZERO
        );
    }

    #[test]
    fn a_split_capture_is_checked_as_a_whole() {
        let mut journal = funded();
        let id = journal
            .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
            .unwrap();
        let fees = account("platform:fees");
        let flight = account("chain:ethereum:in_flight");
        assert_eq!(
            journal.capture_to(
                id,
                &[(flight.clone(), 25), (fees.clone(), 6)],
                Metadata::new()
            ),
            Err(HoldError::ExceedsHold {
                requested: 31,
                held: 30
            })
        );
        assert_eq!(
            journal.capture_to(
                id,
                &[(flight.clone(), 25), (fees.clone(), 0)],
                Metadata::new()
            ),
            Err(HoldError::ZeroAmount)
        );
        assert_eq!(
            journal.capture_to(
                id,
                &[(flight.clone(), u128::MAX), (fees.clone(), 1)],
                Metadata::new()
            ),
            Err(HoldError::ExceedsHold {
                requested: u128::MAX,
                held: 30
            })
        );
        assert_eq!(
            journal.capture_to(id, &[], Metadata::new()),
            Err(HoldError::NoDestinations)
        );
        let too_many: Vec<_> = (0..256).map(|_| (fees.clone(), 1)).collect();
        assert_eq!(
            journal.capture_to(id, &too_many, Metadata::new()),
            Err(HoldError::TooManyDestinations(256))
        );
        assert_eq!(journal.get_hold(id).unwrap().status(), HoldStatus::Active);
    }

    #[test]
    fn void_returns_everything_to_the_owner() {
        let mut journal = funded();
        let id = journal
            .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
            .unwrap();
        journal.void(id, Metadata::new()).unwrap();

        assert_eq!(journal.get_hold(id).unwrap().status(), HoldStatus::Voided);
        assert_eq!(balance(&journal, "users:1:wallet"), amount(100));
    }

    #[test]
    fn a_settled_hold_cannot_be_captured_or_voided_again() {
        let mut journal = funded();
        let id = journal
            .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
            .unwrap();
        journal.void(id, Metadata::new()).unwrap();
        assert_eq!(
            journal.void(id, Metadata::new()),
            Err(HoldError::NotActive(id, HoldStatus::Voided))
        );
        assert_eq!(
            journal.capture(id, account("merchants:7:wallet"), 1, Metadata::new()),
            Err(HoldError::NotActive(id, HoldStatus::Voided))
        );
    }

    #[test]
    fn unknown_hold_is_not_found() {
        let mut journal = funded();
        assert_eq!(
            journal.void(HoldId(9), Metadata::new()),
            Err(HoldError::NotFound(HoldId(9)))
        );
    }

    #[test]
    fn expire_voids_only_holds_past_their_deadline() {
        let mut journal = funded();
        let due = journal
            .hold(
                account("users:1:wallet"),
                10,
                usdc(),
                Some(1_000),
                Metadata::new(),
            )
            .unwrap();
        let later = journal
            .hold(
                account("users:1:wallet"),
                20,
                usdc(),
                Some(2_000),
                Metadata::new(),
            )
            .unwrap();
        let open_ended = journal
            .hold(account("users:1:wallet"), 30, usdc(), None, Metadata::new())
            .unwrap();

        assert_eq!(journal.expire(999), Ok(vec![]));
        assert_eq!(journal.expire(1_000), Ok(vec![due]));

        assert_eq!(journal.get_hold(due).unwrap().status(), HoldStatus::Expired);
        assert_eq!(
            journal.get_hold(later).unwrap().status(),
            HoldStatus::Active
        );
        assert_eq!(
            journal.get_hold(open_ended).unwrap().status(),
            HoldStatus::Active
        );
        assert_eq!(balance(&journal, "users:1:wallet"), amount(50));

        // An expired hold is not expired twice.
        assert_eq!(journal.expire(5_000), Ok(vec![later]));
    }

    #[test]
    fn statuses_round_trip_through_text() {
        for status in [
            HoldStatus::Active,
            HoldStatus::Captured,
            HoldStatus::Voided,
            HoldStatus::Expired,
        ] {
            assert_eq!(status.as_str().parse::<HoldStatus>().unwrap(), status);
        }
        assert!("pending".parse::<HoldStatus>().is_err());
    }

    #[test]
    fn a_hold_can_be_rebuilt_from_storage_and_keeps_its_rules() {
        let owner = account("users:1:wallet");
        assert_eq!(
            Hold::new(
                HoldId(7),
                owner.clone(),
                0,
                usdc(),
                HoldStatus::Active,
                None
            ),
            Err(HoldError::ZeroAmount)
        );
        let captured = Hold::new(
            HoldId(7),
            owner.clone(),
            5,
            usdc(),
            HoldStatus::Captured,
            None,
        )
        .unwrap();
        assert_eq!(captured.account().as_str(), "holds:7");
        assert_eq!(
            captured.release(Metadata::new()),
            Err(HoldError::NotActive(HoldId(7), HoldStatus::Captured))
        );
        let active = Hold::new(HoldId(7), owner, 5, usdc(), HoldStatus::Active, None).unwrap();
        let tx = active
            .capture(account("merchants:1:wallet"), 2, Metadata::new())
            .unwrap();
        assert_eq!(tx.postings().len(), 2);
    }

    #[test]
    fn only_an_active_hold_past_its_deadline_is_due() {
        let owner = account("users:1:wallet");
        let hold = |status, expires_at| {
            Hold::new(HoldId(1), owner.clone(), 5, usdc(), status, expires_at).unwrap()
        };
        assert!(hold(HoldStatus::Active, Some(100)).is_due(100));
        assert!(!hold(HoldStatus::Active, Some(100)).is_due(99));
        assert!(!hold(HoldStatus::Active, None).is_due(u64::MAX));
        assert!(!hold(HoldStatus::Voided, Some(100)).is_due(200));
    }
}
