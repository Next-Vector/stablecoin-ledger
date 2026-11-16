//! Compiles a checked [`Script`] with variable values into one ledger transaction (FR-61–FR-64).
//!
//! Compilation is pure: current balances come from the caller, so a store can run it inside the
//! DB transaction that applies the result. Sends run in order and each sees the balances the
//! earlier ones left.

use std::collections::{BTreeMap, HashMap};

use ledger_core::{
    AccountAddress, AssetId, Balance, Metadata, Posting, PostingError, Transaction,
    TransactionError,
};

use crate::ast::{
    Account, Amount, Asset, MetaValue, Monetary, Portion, Script, Segment, Send, Share, Statement,
    VarType,
};
use crate::ratio::{self, Ratio, SplitError};

/// Variable values as passed in the request, by name (without `$`). Formats: account
/// `users:42:wallet`, asset `USDC@ethereum/6`, monetary `USDC@ethereum/6 100`, number `100`,
/// portion `12.5%` or `1/3`, string anything.
pub type Vars = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    #[error("variable `${0}` has no value")]
    MissingValue(String),
    #[error("variable `${0}` is not declared in the script")]
    UndeclaredValue(String),
    #[error("variable `${name}`: {reason}")]
    InvalidValue { name: String, reason: String },
    #[error("account `{address}` is invalid: {reason}")]
    InvalidAccount { address: String, reason: String },
    #[error("the amount is in {found}, but this send moves {expected}")]
    AssetMismatch { expected: AssetId, found: AssetId },
    #[error("the sources cover {covered} of {needed} {asset}; {missing} missing", missing = needed - covered)]
    InsufficientFunds {
        asset: AssetId,
        covered: u128,
        needed: u128,
    },
    #[error(transparent)]
    Split(#[from] SplitError),
    #[error("a balance would overflow")]
    Overflow,
    #[error("the script produces no postings")]
    NoPostings,
    #[error("the script produces {0} postings; the maximum is {max}", max = Transaction::MAX_POSTINGS)]
    TooManyPostings(usize),
    #[error(transparent)]
    Posting(#[from] PostingError),
}

/// Compiles `script` into a transaction. `balance` returns an account's current balance; only
/// source accounts other than `world` (which is unbounded) are asked.
pub fn compile(
    script: &Script,
    vars: &Vars,
    balance: impl Fn(&AccountAddress, &AssetId) -> Balance,
) -> Result<Transaction, CompileError> {
    let mut compiler = Compiler {
        values: bind(script, vars)?,
        balance,
        changed: HashMap::new(),
        postings: Vec::new(),
        metadata: Metadata::new(),
    };
    for statement in &script.statements {
        match statement {
            Statement::Send(send) => compiler.send(send)?,
            Statement::SetTxMeta { key, value } => {
                let value = match value {
                    MetaValue::Text(text) => text.clone(),
                    MetaValue::Var(name) => compiler.value(name)?.1.clone(),
                };
                compiler.metadata.insert(key.clone(), value);
            }
        }
    }
    Transaction::new(compiler.postings, compiler.metadata).map_err(|error| match error {
        TransactionError::NoPostings => CompileError::NoPostings,
        TransactionError::TooManyPostings(count) => CompileError::TooManyPostings(count),
    })
}

#[derive(Debug, Clone)]
enum Value {
    Account(AccountAddress),
    Asset(AssetId),
    Monetary(AssetId, u128),
    Number(u128),
    Portion(Ratio),
    Text,
}

/// Checks that every declared variable has a well-formed value of its type, and nothing else is
/// passed (FR-61). Keeps the raw text for metadata.
fn bind(script: &Script, vars: &Vars) -> Result<HashMap<String, (Value, String)>, CompileError> {
    if let Some(name) = vars
        .keys()
        .find(|name| !script.vars.iter().any(|decl| &decl.name == *name))
    {
        return Err(CompileError::UndeclaredValue(name.clone()));
    }
    script
        .vars
        .iter()
        .map(|decl| {
            let raw = vars
                .get(&decl.name)
                .ok_or_else(|| CompileError::MissingValue(decl.name.clone()))?;
            let value = parse_value(decl.ty, raw).map_err(|reason| CompileError::InvalidValue {
                name: decl.name.clone(),
                reason,
            })?;
            Ok((decl.name.clone(), (value, raw.clone())))
        })
        .collect()
}

fn parse_value(ty: VarType, raw: &str) -> Result<Value, String> {
    let number = |raw: &str| -> Option<u128> {
        (!raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()))
            .then(|| raw.parse().ok())
            .flatten()
    };
    Ok(match ty {
        VarType::Account => Value::Account(raw.parse().map_err(|e| format!("{e}"))?),
        VarType::Asset => Value::Asset(raw.parse().map_err(|e| format!("{e}"))?),
        VarType::Monetary => {
            let invalid =
                || "monetary must be `ASSET amount`, e.g. `USDC@ethereum/6 100`".to_owned();
            let (asset, amount) = raw.split_once(' ').ok_or_else(invalid)?;
            Value::Monetary(
                asset.parse().map_err(|_| invalid())?,
                number(amount).ok_or_else(invalid)?,
            )
        }
        VarType::Number => {
            Value::Number(number(raw).ok_or("number must be a non-negative integer of base units")?)
        }
        VarType::Portion => Value::Portion(Ratio::parse(raw).map_err(|e| e.to_string())?),
        VarType::String => Value::Text,
    })
}

struct Compiler<F> {
    values: HashMap<String, (Value, String)>,
    balance: F,
    /// Balances changed by the postings compiled so far.
    changed: HashMap<(AccountAddress, AssetId), Balance>,
    postings: Vec<Posting>,
    metadata: Metadata,
}

impl<F: Fn(&AccountAddress, &AssetId) -> Balance> Compiler<F> {
    fn send(&mut self, send: &Send) -> Result<(), CompileError> {
        let (asset, total) = self.monetary(&send.amount)?;

        // Draw from the sources in order: each gives what it can, within its cap.
        let mut needed = total;
        let mut drawn: Vec<(AccountAddress, u128)> = Vec::new();
        for item in &send.source {
            let account = self.account(&item.account)?;
            let cap = match &item.max {
                Some(max) => self.amount_in(max, &asset)?,
                None => u128::MAX,
            };
            let overdraft = match &item.overdraft {
                Some(overdraft) => self.amount_in(overdraft, &asset)?,
                None => 0,
            };
            let available = if account.is_world() {
                u128::MAX
            } else {
                self.current(&account, &asset).spendable(overdraft)
            };
            let take = needed.min(cap).min(available);
            if take > 0 {
                drawn.push((account, take));
                needed -= take;
            }
        }
        if needed > 0 {
            return Err(CompileError::InsufficientFunds {
                asset,
                covered: total - needed,
                needed: total,
            });
        }

        // Split the amount between the destinations, then pair sources with destinations in
        // order: the first source pays the first destinations until it is exhausted, and so on.
        let shares = send
            .destination
            .iter()
            .map(|item| self.share(&item.share))
            .collect::<Result<Vec<_>, _>>()?;
        let amounts = ratio::split(total, &shares)?;
        let mut received: Vec<(AccountAddress, u128)> = Vec::new();
        for (item, amount) in send.destination.iter().zip(amounts) {
            let account = self.account(&item.account)?;
            if amount > 0 {
                received.push((account, amount));
            }
        }
        let (mut sources, mut destinations) = (drawn.into_iter(), received.into_iter());
        let (mut source, mut destination) = (sources.next(), destinations.next());
        while let (Some((from, left_from)), Some((to, left_to))) = (&mut source, &mut destination) {
            let amount = (*left_from).min(*left_to);
            // Moving funds from an account to itself changes nothing.
            if from != to {
                self.post(Posting::new(
                    from.clone(),
                    to.clone(),
                    amount,
                    asset.clone(),
                )?)?;
            }
            *left_from -= amount;
            *left_to -= amount;
            if *left_from == 0 {
                source = sources.next();
            }
            if *left_to == 0 {
                destination = destinations.next();
            }
        }
        Ok(())
    }

    fn post(&mut self, posting: Posting) -> Result<(), CompileError> {
        let asset = posting.asset();
        let debited = self
            .current(posting.source(), asset)
            .debit(posting.amount())
            .map_err(|_| CompileError::Overflow)?;
        let credited = self
            .current(posting.destination(), asset)
            .credit(posting.amount())
            .map_err(|_| CompileError::Overflow)?;
        self.changed
            .insert((posting.source().clone(), asset.clone()), debited);
        self.changed
            .insert((posting.destination().clone(), asset.clone()), credited);
        self.postings.push(posting);
        Ok(())
    }

    fn current(&self, account: &AccountAddress, asset: &AssetId) -> Balance {
        self.changed
            .get(&(account.clone(), asset.clone()))
            .copied()
            .unwrap_or_else(|| (self.balance)(account, asset))
    }

    fn value(&self, name: &str) -> Result<&(Value, String), CompileError> {
        self.values
            .get(name)
            .ok_or_else(|| CompileError::MissingValue(name.to_owned()))
    }

    fn wrong_type(name: &str, expected: VarType) -> CompileError {
        CompileError::InvalidValue {
            name: name.to_owned(),
            reason: format!("expected a {} value", expected.keyword()),
        }
    }

    fn monetary(&self, monetary: &Monetary) -> Result<(AssetId, u128), CompileError> {
        match monetary {
            Monetary::Var(name) => match &self.value(name)?.0 {
                Value::Monetary(asset, amount) => Ok((asset.clone(), *amount)),
                _ => Err(Self::wrong_type(name, VarType::Monetary)),
            },
            Monetary::Literal { asset, amount } => {
                let asset = match asset {
                    Asset::Literal(asset) => asset.clone(),
                    Asset::Var(name) => match &self.value(name)?.0 {
                        Value::Asset(asset) => asset.clone(),
                        _ => return Err(Self::wrong_type(name, VarType::Asset)),
                    },
                };
                let amount = match amount {
                    Amount::Literal(amount) => *amount,
                    Amount::Var(name) => match &self.value(name)?.0 {
                        Value::Number(amount) => *amount,
                        _ => return Err(Self::wrong_type(name, VarType::Number)),
                    },
                };
                Ok((asset, amount))
            }
        }
    }

    /// A cap or overdraft, which must be in the asset being sent.
    fn amount_in(&self, monetary: &Monetary, expected: &AssetId) -> Result<u128, CompileError> {
        let (asset, amount) = self.monetary(monetary)?;
        if &asset != expected {
            return Err(CompileError::AssetMismatch {
                expected: expected.clone(),
                found: asset,
            });
        }
        Ok(amount)
    }

    fn account(&self, account: &Account) -> Result<AccountAddress, CompileError> {
        let segments = match account {
            Account::Var(name) => {
                return match &self.value(name)?.0 {
                    Value::Account(address) => Ok(address.clone()),
                    _ => Err(Self::wrong_type(name, VarType::Account)),
                };
            }
            Account::Template(segments) => segments,
        };
        let mut parts = Vec::with_capacity(segments.len());
        for segment in segments {
            parts.push(match segment {
                Segment::Text(text) => text.clone(),
                Segment::Var(name) => match self.value(name)? {
                    (Value::Account(address), _) => address.to_string(),
                    (Value::Number(number), _) => number.to_string(),
                    (Value::Text, raw) => raw.clone(),
                    _ => return Err(Self::wrong_type(name, VarType::Account)),
                },
            });
        }
        let address = parts.join(":");
        address
            .parse()
            .map_err(
                |error: ledger_core::AccountAddressError| CompileError::InvalidAccount {
                    address,
                    reason: error.to_string(),
                },
            )
    }

    fn share(&self, share: &Share) -> Result<ratio::Share, CompileError> {
        Ok(match share {
            Share::Remaining => ratio::Share::Remaining,
            Share::Portion(Portion::Literal(ratio)) => ratio::Share::Portion(*ratio),
            Share::Portion(Portion::Var(name)) => match &self.value(name)?.0 {
                Value::Portion(ratio) => ratio::Share::Portion(*ratio),
                _ => return Err(Self::wrong_type(name, VarType::Portion)),
            },
        })
    }
}
