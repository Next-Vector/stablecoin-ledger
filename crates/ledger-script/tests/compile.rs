//! Properties of the compiler as a whole: what the destinations receive always equals what the
//! sources give and the amount sent (FR-63), and the posting limit holds (FR-64).

use std::collections::HashMap;

use ledger_core::{AccountAddress, AssetId, Balance};
use proptest::prelude::*;

fn compile(
    source: &str,
    balances: &HashMap<String, u128>,
) -> Result<ledger_core::Transaction, String> {
    let script = ledger_script::parse(source).map_err(|e| e.to_string())?;
    ledger_script::compile(
        &script,
        &ledger_script::Vars::new(),
        |account: &AccountAddress, _: &AssetId| {
            balances
                .get(account.as_str())
                .map(|&amount| Balance::ZERO.credit(amount).unwrap())
                .unwrap_or_default()
        },
    )
    .map_err(|e| e.to_string())
}

proptest! {
    #[test]
    fn destinations_receive_exactly_the_amount_sent(
        total in 0u128..=u128::MAX / 2,
        bonus in any::<u64>(),
        percents in prop::collection::vec(0u32..=10_000, 1..8),
        with_remaining: bool,
    ) {
        // Percent shares with two decimals; without `remaining`, the last one completes 100%.
        let mut left = 10_000u32;
        let mut shares = Vec::new();
        for (i, &p) in percents.iter().enumerate() {
            let p = p.min(left);
            left -= p;
            shares.push(format!("{}.{:02}% to @dest:d{i}", p / 100, p % 100));
        }
        let last = if with_remaining {
            "remaining to @dest:rest".to_owned()
        } else {
            format!("{}.{:02}% to @dest:rest", left / 100, left % 100)
        };
        let source = format!(
            "send [USDC@ethereum/6 {total}] (\n  source = {{ @users:1:bonus @world }}\n  destination = {{\n    {}\n    {last}\n  }}\n)",
            shares.join("\n    ")
        );
        let balances = HashMap::from([("users:1:bonus".to_owned(), u128::from(bonus))]);
        match compile(&source, &balances) {
            Ok(tx) => {
                let sent: u128 = tx.postings().iter().map(|p| p.amount()).sum();
                prop_assert_eq!(sent, total);
                let from_bonus: u128 = tx
                    .postings()
                    .iter()
                    .filter(|p| p.source().as_str() == "users:1:bonus")
                    .map(|p| p.amount())
                    .sum();
                prop_assert_eq!(from_bonus, total.min(u128::from(bonus)));
                prop_assert!(tx.postings().iter().all(|p| p.destination().as_str().starts_with("dest:")));
            }
            Err(error) => prop_assert_eq!(error, "the script produces no postings"),
        }
    }
}

#[test]
fn more_than_256_postings_are_rejected() {
    let destinations: Vec<String> = (0..257).map(|i| format!("1/257 to @d:{i}")).collect();
    let source = format!(
        "send [USDC@ethereum/6 257] (source = @world destination = {{ {} }})",
        destinations.join(" ")
    );
    assert_eq!(
        compile(&source, &HashMap::new()).unwrap_err(),
        "the script produces 257 postings; the maximum is 256"
    );
    let fits = source.replace("257", "256").replace(" 1/256 to @d:256", "");
    assert_eq!(
        compile(&fits, &HashMap::new()).unwrap().postings().len(),
        256
    );
}
