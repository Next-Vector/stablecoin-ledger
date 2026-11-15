//! Property tests for the ledger invariants (spec §4.3):
//! - for every asset the sum of all balances, `world` included, is zero;
//! - no account except `world` ever has a negative balance;
//! - a rejected operation leaves every balance unchanged.
//!
//! 100 cases × 100 random operations = 10,000 operations per run (acceptance criterion, spec §10).

use std::collections::BTreeMap;

use ledger_core::{
    AccountAddress, AssetId, Balance, HoldId, Journal, Metadata, Posting, Transaction, TxId,
};
use proptest::prelude::*;

const CASES: u32 = 100;
const OPERATIONS_PER_CASE: usize = 100;

const ACCOUNTS: [&str; 8] = [
    "world",
    "users:0:wallet",
    "users:1:wallet",
    "users:2:wallet",
    "users:3:wallet",
    "users:4:wallet",
    "merchants:0:wallet",
    "platform:fees",
];
const ASSETS: [&str; 2] = ["USDC@ethereum/6", "DAI@ethereum/18"];

#[derive(Debug, Clone)]
enum Op {
    Transfer(Vec<(usize, usize, u128, usize)>),
    Revert(usize),
    Hold {
        owner: usize,
        amount: u128,
        asset: usize,
        expires_at: Option<u64>,
    },
    Capture {
        hold: usize,
        destination: usize,
        amount: u128,
    },
    Void(usize),
    Expire(u64),
    Freeze(usize),
    Unfreeze(usize),
}

fn amount() -> impl Strategy<Value = u128> {
    // Mostly small values, plus huge ones that drive balances towards overflow.
    prop_oneof![8 => 1..=1_000u128, 1 => (u128::MAX / 2)..=u128::MAX]
}

fn op() -> impl Strategy<Value = Op> {
    let account = 0..ACCOUNTS.len();
    let asset = 0..ASSETS.len();
    prop_oneof![
        6 => prop::collection::vec((account.clone(), account.clone(), amount(), asset.clone()), 1..=3).prop_map(Op::Transfer),
        1 => any::<usize>().prop_map(Op::Revert),
        2 => (account.clone(), amount(), asset, prop::option::of(0..100u64))
            .prop_map(|(owner, amount, asset, expires_at)| Op::Hold { owner, amount, asset, expires_at }),
        2 => (any::<usize>(), account.clone(), amount())
            .prop_map(|(hold, destination, amount)| Op::Capture { hold, destination, amount }),
        1 => any::<usize>().prop_map(Op::Void),
        1 => (0..100u64).prop_map(Op::Expire),
        1 => account.clone().prop_map(Op::Freeze),
        1 => account.prop_map(Op::Unfreeze),
    ]
}

fn address(index: usize) -> AccountAddress {
    ACCOUNTS[index].parse().unwrap()
}

fn asset(index: usize) -> AssetId {
    ASSETS[index].parse().unwrap()
}

/// Drives a journal with generated operations, tracking the ids it needs to address later.
#[derive(Default)]
struct Model {
    journal: Journal,
    transactions: Vec<TxId>,
    holds: Vec<HoldId>,
}

impl Model {
    /// Applies `op` and reports whether the journal accepted it.
    fn run(&mut self, op: &Op) -> bool {
        match op {
            Op::Transfer(legs) => {
                let postings: Option<Vec<Posting>> = legs
                    .iter()
                    .map(|&(from, to, amount, a)| {
                        Posting::new(address(from), address(to), amount, asset(a)).ok()
                    })
                    .collect();
                let Some(tx) = postings.and_then(|p| Transaction::new(p, Metadata::new()).ok())
                else {
                    return false;
                };
                track(&mut self.transactions, self.journal.apply(tx).ok())
            }
            Op::Revert(pick) => match self.pick(&self.transactions.clone(), *pick) {
                Some(id) => track(
                    &mut self.transactions,
                    self.journal.revert(id, Metadata::new()).ok(),
                ),
                None => false,
            },
            Op::Hold {
                owner,
                amount,
                asset: a,
                expires_at,
            } => {
                match self.journal.hold(
                    address(*owner),
                    *amount,
                    asset(*a),
                    *expires_at,
                    Metadata::new(),
                ) {
                    Ok(id) => {
                        self.holds.push(id);
                        true
                    }
                    Err(_) => false,
                }
            }
            Op::Capture {
                hold,
                destination,
                amount,
            } => match self.pick(&self.holds.clone(), *hold) {
                Some(id) => track(
                    &mut self.transactions,
                    self.journal
                        .capture(id, address(*destination), *amount, Metadata::new())
                        .ok(),
                ),
                None => false,
            },
            Op::Void(hold) => match self.pick(&self.holds.clone(), *hold) {
                Some(id) => track(
                    &mut self.transactions,
                    self.journal.void(id, Metadata::new()).ok(),
                ),
                None => false,
            },
            Op::Expire(now) => self.journal.expire(*now).is_ok(),
            Op::Freeze(account) => {
                self.journal.freeze(address(*account));
                true
            }
            Op::Unfreeze(account) => {
                self.journal.unfreeze(&address(*account));
                true
            }
        }
    }

    fn pick<T: Copy>(&self, items: &[T], pick: usize) -> Option<T> {
        (!items.is_empty()).then(|| items[pick % items.len()])
    }

    /// Every account the journal may hold money on: the fixed set plus each hold's account.
    fn accounts(&self) -> Vec<AccountAddress> {
        let holds = self
            .holds
            .iter()
            .filter_map(|&id| self.journal.get_hold(id))
            .map(|hold| hold.account().clone());
        (0..ACCOUNTS.len()).map(address).chain(holds).collect()
    }

    fn snapshot(&self) -> BTreeMap<(String, String), Balance> {
        let mut balances = BTreeMap::new();
        for account in self.accounts() {
            for raw in ASSETS {
                let asset: AssetId = raw.parse().unwrap();
                balances.insert(
                    (account.to_string(), raw.to_owned()),
                    self.journal.balance(&account, &asset),
                );
            }
        }
        balances
    }
}

/// Records an accepted transaction id and reports whether the operation was accepted.
fn track(transactions: &mut Vec<TxId>, id: Option<TxId>) -> bool {
    if let Some(id) = id {
        transactions.push(id);
    }
    id.is_some()
}

fn check_invariants(model: &Model) -> Result<(), TestCaseError> {
    let accounts = model.accounts();
    for raw in ASSETS {
        let asset: AssetId = raw.parse().unwrap();
        // `world` first: it carries the whole negative side, so partial sums never overflow.
        let mut sum = Balance::ZERO;
        for account in &accounts {
            let balance = model.journal.balance(account, &asset);
            prop_assert!(
                account.is_world() || !balance.is_negative(),
                "{account} has a negative balance {balance} in {raw}"
            );
            sum = sum
                .checked_add(balance)
                .map_err(|e| TestCaseError::fail(format!("sum overflow: {e}")))?;
        }
        prop_assert!(sum.is_zero(), "balances of {raw} sum to {sum}, not zero");
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    #[test]
    fn invariants_hold_for_random_operation_sequences(ops in prop::collection::vec(op(), OPERATIONS_PER_CASE)) {
        let mut model = Model::default();
        for op in &ops {
            let before = model.snapshot();
            let accepted = model.run(op);
            if !accepted {
                prop_assert_eq!(&model.snapshot(), &before, "rejected {:?} changed balances", op);
            }
            check_invariants(&model)?;
        }
    }
}
