//! Transaction DSL (spec §5.7): parser and compiler to ledger postings.
//!
//! The language has no loops, recursion, network or time access (FR-64), so every script
//! terminates and evaluates deterministically.

pub mod ast;
mod compile;
mod error;
mod lexer;
mod parser;
mod ratio;

pub use compile::{CompileError, Vars, compile, touched};
pub use error::ParseError;
pub use parser::{MAX_SCRIPT_BYTES, parse};
pub use ratio::{RatioError, SplitError};
