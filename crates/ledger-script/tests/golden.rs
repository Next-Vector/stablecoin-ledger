//! Golden tests (FR-60, FR-62, FR-63): every `tests/golden/ok/*.lsc` parses to the AST in its
//! `.ast` file, every `tests/golden/errors/*.lsc` fails with the message in its `.err` file, and
//! every `tests/golden/compile/ok/*.lsc` compiles, with the variables and balances in its `.in`
//! file, to the postings in its `.out` file (`compile/errors`: to the error in `.err`).
//! Expected postings and messages are written by hand.
//! `UPDATE_GOLDEN=1 cargo test -p ledger-script --test golden` rewrites the `.ast` files;
//! review the diff before committing. Error messages are always written by hand.

use std::fs;
use std::path::{Path, PathBuf};

fn cases(dir: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(dir);
    let mut cases: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "lsc"))
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no cases in {}", dir.display());
    cases
}

#[test]
fn valid_scripts_parse_to_their_golden_ast() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut failures = Vec::new();
    for case in cases("ok") {
        let source = fs::read_to_string(&case).unwrap();
        let ast = match ledger_script::parse(&source) {
            Ok(script) => format!("{script:#?}\n"),
            Err(error) => {
                failures.push(format!("{}: {error}", case.display()));
                continue;
            }
        };
        let golden = case.with_extension("ast");
        if update {
            fs::write(&golden, &ast).unwrap();
        } else if fs::read_to_string(&golden).ok().as_deref() != Some(ast.as_str()) {
            failures.push(format!(
                "{}: AST differs from {}",
                case.display(),
                golden.display()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn invalid_scripts_fail_with_their_golden_message() {
    let mut failures = Vec::new();
    for case in cases("errors") {
        let source = fs::read_to_string(&case).unwrap();
        let expected = fs::read_to_string(case.with_extension("err")).unwrap();
        match ledger_script::parse(&source) {
            Ok(_) => failures.push(format!("{}: parsed, expected an error", case.display())),
            Err(error) if error.to_string() == expected.trim_end() => {}
            Err(error) => failures.push(format!(
                "{}:\n  expected: {}\n  actual:   {error}",
                case.file_name().unwrap().to_string_lossy(),
                expected.trim_end()
            )),
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn rejects_scripts_over_64_kib_before_parsing() {
    let mut source = "// padding\n".repeat(6000);
    source.push_str("send [USDC@ethereum/6 1] (source = @world destination = @a)\n");
    assert!(source.len() > ledger_script::MAX_SCRIPT_BYTES);
    let error = ledger_script::parse(&source).unwrap_err();
    assert_eq!(error.span.line, 1);
    assert!(error.message.contains("the maximum is 65536"), "{error}");

    let padding = ledger_script::MAX_SCRIPT_BYTES - 100;
    let fits = format!(
        "{}\nsend [USDC@ethereum/6 1] (source = @world destination = @a)\n",
        "/".repeat(padding)
    );
    assert!(fits.len() <= ledger_script::MAX_SCRIPT_BYTES);
    assert!(ledger_script::parse(&fits).is_ok());
}

/// Reads `var name = value` and `balance account asset amount` lines.
fn compile_case(case: &Path) -> Result<String, String> {
    let source = fs::read_to_string(case).unwrap();
    let input = fs::read_to_string(case.with_extension("in")).unwrap();
    let mut vars = ledger_script::Vars::new();
    let mut balances = std::collections::HashMap::new();
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(rest) = line.strip_prefix("var ") {
            let (name, value) = rest.split_once(" = ").expect("var name = value");
            vars.insert(name.to_owned(), value.to_owned());
        } else if let Some(rest) = line.strip_prefix("balance ") {
            let parts: Vec<&str> = rest.split(' ').collect();
            let [account, asset, amount] = parts[..] else {
                panic!("balance account asset amount: {line}")
            };
            let account: ledger_core::AccountAddress = account.parse().unwrap();
            let asset: ledger_core::AssetId = asset.parse().unwrap();
            balances.insert(
                (account, asset),
                amount.parse::<ledger_core::Balance>().unwrap(),
            );
        } else {
            panic!("unknown input line: {line}");
        }
    }
    let script = ledger_script::parse(&source).map_err(|error| error.to_string())?;
    let tx = ledger_script::compile(&script, &vars, |account, asset| {
        balances
            .get(&(account.clone(), asset.clone()))
            .copied()
            .unwrap_or_default()
    })
    .map_err(|error| error.to_string())?;
    let mut out = String::new();
    for p in tx.postings() {
        out.push_str(&format!(
            "{} -> {} {} {}\n",
            p.source(),
            p.destination(),
            p.amount(),
            p.asset()
        ));
    }
    for (key, value) in tx.metadata() {
        out.push_str(&format!("meta {key} = {value}\n"));
    }
    Ok(out)
}

#[test]
fn scripts_compile_to_their_golden_postings() {
    let mut failures = Vec::new();
    for case in cases("compile/ok") {
        let expected = fs::read_to_string(case.with_extension("out")).unwrap();
        match compile_case(&case) {
            Ok(actual) if actual == expected => {}
            Ok(actual) => failures.push(format!(
                "{}:\n--- expected\n{expected}--- actual\n{actual}",
                case.file_name().unwrap().to_string_lossy()
            )),
            Err(error) => failures.push(format!(
                "{}: {error}",
                case.file_name().unwrap().to_string_lossy()
            )),
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn scripts_fail_to_compile_with_their_golden_message() {
    let mut failures = Vec::new();
    for case in cases("compile/errors") {
        let expected = fs::read_to_string(case.with_extension("err")).unwrap();
        match compile_case(&case) {
            Err(error) if error == expected.trim_end() => {}
            other => failures.push(format!(
                "{}:\n  expected: {}\n  actual:   {other:?}",
                case.file_name().unwrap().to_string_lossy(),
                expected.trim_end()
            )),
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
