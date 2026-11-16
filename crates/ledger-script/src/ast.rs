//! The syntax tree of a ledger script. It is checked when parsed: every variable is declared
//! once and used where its type fits, `remaining` comes last, and literal portions are ≤ 100%.

use ledger_core::AssetId;

/// A position in the script text, 1-based; columns count characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub vars: Vec<VarDecl>,
    pub statements: Vec<Statement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarDecl {
    pub name: String,
    pub ty: VarType,
}

/// The types a variable can have (FR-61); values are passed with the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarType {
    Account,
    Asset,
    Monetary,
    Number,
    Portion,
    String,
}

impl VarType {
    pub fn keyword(self) -> &'static str {
        match self {
            VarType::Account => "account",
            VarType::Asset => "asset",
            VarType::Monetary => "monetary",
            VarType::Number => "number",
            VarType::Portion => "portion",
            VarType::String => "string",
        }
    }

    pub(crate) fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "account" => VarType::Account,
            "asset" => VarType::Asset,
            "monetary" => VarType::Monetary,
            "number" => VarType::Number,
            "portion" => VarType::Portion,
            "string" => VarType::String,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Send(Send),
    /// `set_tx_meta("key", value)`: adds a key to the transaction metadata.
    SetTxMeta {
        key: String,
        value: MetaValue,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaValue {
    Text(String),
    Var(String),
}

/// `send <monetary> ( source = … destination = … )`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Send {
    pub amount: Monetary,
    /// Drawn from in order until the amount is covered.
    pub source: Vec<SourceItem>,
    /// Shares of the amount; a single account receives everything (`Share::Remaining`).
    pub destination: Vec<DestinationItem>,
}

/// An amount of an asset: `[USDC@ethereum/6 5000000]` or a `monetary` variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Monetary {
    Literal { asset: Asset, amount: Amount },
    Var(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asset {
    Literal(AssetId),
    Var(String),
}

/// Base units: a literal or a `number` variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Amount {
    Literal(u128),
    Var(String),
}

/// An account address: `@users:{$buyer}:wallet` or an `account` variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Account {
    Template(Vec<Segment>),
    Var(String),
}

/// One `:`-separated part of an address template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    /// `{$name}`: the variable's value becomes the segment.
    Var(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceItem {
    pub account: Account,
    /// `max [ASSET n]`: take at most this much from the account.
    pub max: Option<Monetary>,
    /// `allowing overdraft up to [ASSET n]`: the account may go this far below zero.
    pub overdraft: Option<Monetary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationItem {
    pub share: Share,
    pub account: Account,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Share {
    Portion(Portion),
    /// Whatever the other shares leave.
    Remaining,
}

/// A part of the whole: `12.5%`, `1/3` or a `portion` variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Portion {
    Ratio { numerator: u128, denominator: u128 },
    Var(String),
}
