//! The journal rules as a pure function, shared by every store (in-memory and PostgreSQL), so
//! funds, frozen-account and overflow checks live in exactly one place.

use std::collections::BTreeMap;

use crate::{AccountAddress, ApplyError, AssetId, Balance, Transaction};

/// The balance an account will have in one asset once a transaction is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceChange {
    pub account: AccountAddress,
    pub asset: AssetId,
    pub balance: Balance,
}

/// Checks `tx` against the current state and returns the new balance of every touched
/// `(account, asset)`, sorted by account then asset — the order stores lock rows in.
///
/// Rules: a frozen account cannot be a source; overflow rejects the transaction; after all
/// postings, no account may be negative except `world` and accounts flagged by
/// `may_go_negative` (system accounts such as gas expenses; checked on the net result).
pub fn plan(
    tx: &Transaction,
    current: impl Fn(&AccountAddress, &AssetId) -> Balance,
    is_frozen: impl Fn(&AccountAddress) -> bool,
    may_go_negative: impl Fn(&AccountAddress) -> bool,
) -> Result<Vec<BalanceChange>, ApplyError> {
    let mut staged: BTreeMap<(AccountAddress, AssetId), Balance> = BTreeMap::new();
    for posting in tx.postings() {
        if is_frozen(posting.source()) {
            return Err(ApplyError::AccountFrozen(posting.source().clone()));
        }
        let source = (posting.source().clone(), posting.asset().clone());
        let destination = (posting.destination().clone(), posting.asset().clone());
        let debited = staged_balance(&staged, &source, &current).debit(posting.amount())?;
        staged.insert(source, debited);
        let credited = staged_balance(&staged, &destination, &current).credit(posting.amount())?;
        staged.insert(destination, credited);
    }

    if let Some(((account, asset), _)) = staged.iter().find(|((account, _), balance)| {
        balance.is_negative() && !account.is_world() && !may_go_negative(account)
    }) {
        return Err(ApplyError::InsufficientFunds {
            account: account.clone(),
            asset: asset.clone(),
        });
    }

    Ok(staged
        .into_iter()
        .map(|((account, asset), balance)| BalanceChange {
            account,
            asset,
            balance,
        })
        .collect())
}

fn staged_balance(
    staged: &BTreeMap<(AccountAddress, AssetId), Balance>,
    key: &(AccountAddress, AssetId),
    current: &impl Fn(&AccountAddress, &AssetId) -> Balance,
) -> Balance {
    staged
        .get(key)
        .copied()
        .unwrap_or_else(|| current(&key.0, &key.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Metadata, Posting};

    fn account(raw: &str) -> AccountAddress {
        raw.parse().unwrap()
    }

    fn usdc() -> AssetId {
        "USDC@ethereum/6".parse().unwrap()
    }

    fn tx(postings: &[(&str, &str, u128)]) -> Transaction {
        let postings = postings
            .iter()
            .map(|&(from, to, amount)| {
                Posting::new(account(from), account(to), amount, usdc()).unwrap()
            })
            .collect();
        Transaction::new(postings, Metadata::new()).unwrap()
    }

    fn balance(value: i128) -> Balance {
        if value < 0 {
            Balance::ZERO.debit(value.unsigned_abs()).unwrap()
        } else {
            Balance::ZERO.credit(value as u128).unwrap()
        }
    }

    fn funded(balances: &[(&str, i128)]) -> impl Fn(&AccountAddress, &AssetId) -> Balance {
        let balances: Vec<(AccountAddress, Balance)> = balances
            .iter()
            .map(|&(raw, value)| (account(raw), balance(value)))
            .collect();
        move |account, _| {
            balances
                .iter()
                .find(|(known, _)| known == account)
                .map(|(_, b)| *b)
                .unwrap_or_default()
        }
    }

    fn not_frozen(_: &AccountAddress) -> bool {
        false
    }

    /// No account besides `world` is flagged to go negative.
    fn only_world(_: &AccountAddress) -> bool {
        false
    }

    #[test]
    fn a_flagged_system_account_may_go_negative() {
        // Gas is spent from the platform's own account: an expense, so it goes negative.
        let gas = |a: &AccountAddress| a.as_str() == "platform:gas:ethereum";
        let changes = plan(
            &tx(&[("platform:gas:ethereum", "world", 7)]),
            funded(&[]),
            not_frozen,
            gas,
        )
        .unwrap();
        assert!(
            changes
                .iter()
                .any(|c| c.account.as_str() == "platform:gas:ethereum" && c.balance == balance(-7))
        );

        let unflagged = plan(
            &tx(&[("platform:fees", "world", 7)]),
            funded(&[]),
            not_frozen,
            gas,
        );
        assert_eq!(
            unflagged,
            Err(ApplyError::InsufficientFunds {
                account: account("platform:fees"),
                asset: usdc()
            })
        );
    }

    #[test]
    fn returns_the_new_balance_of_every_touched_account_sorted_by_key() {
        let changes = plan(
            &tx(&[
                ("users:1:wallet", "merchants:7:wallet", 99),
                ("users:1:wallet", "platform:fees", 1),
            ]),
            funded(&[("users:1:wallet", 100), ("merchants:7:wallet", 5)]),
            not_frozen,
            only_world,
        )
        .unwrap();

        let summary: Vec<(String, Balance)> = changes
            .iter()
            .map(|c| (c.account.to_string(), c.balance))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("merchants:7:wallet".to_owned(), balance(104)),
                ("platform:fees".to_owned(), balance(1)),
                ("users:1:wallet".to_owned(), Balance::ZERO),
            ]
        );
    }

    #[test]
    fn checks_funds_on_the_net_result() {
        let changes = plan(
            &tx(&[
                ("world", "users:1:wallet", 50),
                ("users:1:wallet", "users:2:wallet", 50),
            ]),
            funded(&[]),
            not_frozen,
            only_world,
        )
        .unwrap();
        assert!(
            changes
                .iter()
                .any(|c| c.account.is_world() && c.balance == balance(-50))
        );
    }

    #[test]
    fn rejects_insufficient_funds() {
        let result = plan(
            &tx(&[("users:1:wallet", "users:2:wallet", 101)]),
            funded(&[("users:1:wallet", 100)]),
            not_frozen,
            only_world,
        );
        assert_eq!(
            result,
            Err(ApplyError::InsufficientFunds {
                account: account("users:1:wallet"),
                asset: usdc()
            })
        );
    }

    #[test]
    fn rejects_a_frozen_source_but_not_a_frozen_destination() {
        let frozen = |a: &AccountAddress| a.as_str() == "users:1:wallet";
        let result = plan(
            &tx(&[("users:1:wallet", "users:2:wallet", 1)]),
            funded(&[("users:1:wallet", 10)]),
            frozen,
            only_world,
        );
        assert_eq!(
            result,
            Err(ApplyError::AccountFrozen(account("users:1:wallet")))
        );

        let incoming = plan(
            &tx(&[("users:2:wallet", "users:1:wallet", 1)]),
            funded(&[("users:2:wallet", 10)]),
            frozen,
            only_world,
        );
        assert!(incoming.is_ok());
    }

    #[test]
    fn reports_the_first_overdrawn_account_in_key_order() {
        let result = plan(
            &tx(&[
                ("users:2:wallet", "world", 1),
                ("users:1:wallet", "world", 1),
            ]),
            funded(&[]),
            not_frozen,
            only_world,
        );
        assert_eq!(
            result,
            Err(ApplyError::InsufficientFunds {
                account: account("users:1:wallet"),
                asset: usdc()
            })
        );
    }

    #[test]
    fn rejects_overflow() {
        let result = plan(
            &tx(&[("world", "users:1:wallet", 1)]),
            funded(&[("users:1:wallet", i128::MAX)]),
            not_frozen,
            only_world,
        );
        assert!(result.is_ok());
        let full = |a: &AccountAddress, _: &AssetId| {
            if a.is_world() {
                Balance::ZERO
            } else {
                Balance::ZERO.credit(u128::MAX).unwrap()
            }
        };
        assert_eq!(
            plan(
                &tx(&[("world", "users:1:wallet", 1)]),
                full,
                not_frozen,
                only_world
            ),
            Err(ApplyError::Overflow)
        );
    }
}
