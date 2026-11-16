//! Compiling any script that parses must never panic, must be deterministic, and must only move
//! the `(account, asset)` pairs `touched` reports — the rows the store locks before compiling.
//! Variable values and balances are derived from a hash of the input, so seeds from the golden
//! scripts compile meaningfully.

#![no_main]

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use ledger_core::{AccountAddress, AssetId, Balance};
use ledger_script::ast::VarType;
use libfuzzer_sys::fuzz_target;

fn pick<'a>(options: &[&'a str], seed: u64) -> &'a str {
    options[(seed % options.len() as u64) as usize]
}

fn value(ty: VarType, seed: u64) -> String {
    let value = match ty {
        VarType::Account => pick(&["42", "users:1:wallet", "world", "a", "Bad", "x:y"], seed),
        VarType::Asset => pick(&["USDC@ethereum/6", "DAI@ethereum/18", "usdc"], seed),
        VarType::Monetary => pick(
            &[
                "USDC@ethereum/6 100",
                "USDC@ethereum/6 0",
                "DAI@ethereum/18 340282366920938463463374607431768211455",
                "USDC@ethereum/6",
            ],
            seed,
        ),
        VarType::Number => pick(
            &[
                "0",
                "1",
                "100",
                "340282366920938463463374607431768211455",
                "-1",
            ],
            seed,
        ),
        VarType::Portion => pick(&["0%", "50%", "1/3", "100%", "12.5%", "2/1"], seed),
        VarType::String => pick(&["x", "42", "a b", ""], seed),
    };
    value.to_owned()
}

fn hash(parts: &[&str], seed: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    parts.hash(&mut hasher);
    hasher.finish()
}

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(script) = ledger_script::parse(source) else {
        return;
    };
    let seed = hash(&[source], 0);
    let vars: BTreeMap<String, String> = script
        .vars
        .iter()
        .enumerate()
        .map(|(i, decl)| {
            (
                decl.name.clone(),
                value(decl.ty, seed.rotate_left(i as u32 * 7)),
            )
        })
        .collect();
    let balance = |account: &AccountAddress, asset: &AssetId| {
        let h = hash(&[account.as_str(), &asset.to_string()], seed);
        let magnitude = u128::from(h % 1_000);
        if h & (1 << 20) == 0 {
            Balance::ZERO.credit(magnitude).unwrap()
        } else {
            Balance::ZERO.debit(magnitude).unwrap()
        }
    };

    let first = ledger_script::compile(&script, &vars, balance);
    let second = ledger_script::compile(&script, &vars, balance);
    assert_eq!(first, second, "compilation is not deterministic");

    if let Ok(tx) = first {
        let touched =
            ledger_script::touched(&script, &vars).expect("compiled, so touched resolves");
        for posting in tx.postings() {
            for account in [posting.source(), posting.destination()] {
                assert!(
                    touched.contains(&(account.clone(), posting.asset().clone())),
                    "{account} {} moved but not reported by touched",
                    posting.asset()
                );
            }
        }
        assert!(tx.postings().len() <= ledger_core::Transaction::MAX_POSTINGS);
    }
});
