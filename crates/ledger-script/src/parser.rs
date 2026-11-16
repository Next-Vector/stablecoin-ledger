//! Recursive-descent parser from tokens to a checked [`Script`]. Grammar:
//!
//! ```text
//! script      = [ "vars" "{" { type $name } "}" ] statement { statement }
//! statement   = send | "set_tx_meta" "(" string "," ( string | $var ) ")"
//! send        = "send" monetary "(" "source" "=" source "destination" "=" destination ")"
//! source      = item | "{" item { item } "}"
//! item        = account [ "max" monetary ] [ "allowing" "overdraft" "up" "to" monetary ]
//! destination = account | "{" share "to" account { share "to" account } "}"
//! share       = number "%" | number "/" number | $portion | "remaining"
//! monetary    = "[" ( ASSET | $asset ) ( number | $number ) "]" | $monetary
//! account     = "@" segment { ":" segment } | $account      segment = word | "{" $var "}"
//! ```

use std::collections::HashMap;

use crate::ast::{
    Account, Amount, Asset, DestinationItem, MetaValue, Monetary, Portion, Ratio, Script, Segment,
    Send, Share, SourceItem, Span, Statement, VarDecl, VarType,
};
use crate::error::ParseError;
use crate::lexer::{Token, tokenize};

/// Upper bound on script size (FR-64).
pub const MAX_SCRIPT_BYTES: usize = 64 * 1024;

/// Parses and checks a script.
pub fn parse(source: &str) -> Result<Script, ParseError> {
    if source.len() > MAX_SCRIPT_BYTES {
        return Err(ParseError::new(
            Span { line: 1, column: 1 },
            format!(
                "script is {} bytes; the maximum is {MAX_SCRIPT_BYTES}",
                source.len()
            ),
        ));
    }
    let mut parser = Parser {
        tokens: tokenize(source)?,
        pos: 0,
        vars: HashMap::new(),
    };
    parser.script()
}

struct Parser {
    tokens: Vec<(Token, Span)>,
    pos: usize,
    vars: HashMap<String, VarType>,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos].0
    }

    fn span(&self) -> Span {
        self.tokens[self.pos].1
    }

    fn advance(&mut self) -> (Token, Span) {
        let current = self.tokens[self.pos].clone();
        if current.0 != Token::End {
            self.pos += 1;
        }
        current
    }

    fn at_word(&self, word: &str) -> bool {
        matches!(self.peek(), Token::Word(w) if w == word)
    }

    fn at_symbol(&self, symbol: char) -> bool {
        *self.peek() == Token::Symbol(symbol)
    }

    /// An "expected …, found …" error at the current token.
    fn expected(&self, what: &str) -> ParseError {
        ParseError::new(
            self.span(),
            format!("expected {what}, found {}", self.peek().describe()),
        )
    }

    fn word(&mut self, word: &str) -> Result<(), ParseError> {
        if !self.at_word(word) {
            return Err(self.expected(&format!("`{word}`")));
        }
        self.advance();
        Ok(())
    }

    fn symbol(&mut self, symbol: char) -> Result<(), ParseError> {
        if !self.at_symbol(symbol) {
            return Err(self.expected(&format!("`{symbol}`")));
        }
        self.advance();
        Ok(())
    }

    fn script(&mut self) -> Result<Script, ParseError> {
        let vars = if self.at_word("vars") {
            self.vars_block()?
        } else {
            Vec::new()
        };
        let mut statements = Vec::new();
        while *self.peek() != Token::End {
            statements.push(self.statement()?);
        }
        if statements.is_empty() {
            return Err(self.expected("`send` or `set_tx_meta`"));
        }
        Ok(Script { vars, statements })
    }

    fn vars_block(&mut self) -> Result<Vec<VarDecl>, ParseError> {
        self.word("vars")?;
        self.symbol('{')?;
        let mut vars = Vec::new();
        while !self.at_symbol('}') {
            let ty = match self.peek() {
                Token::Word(word) => VarType::parse(word),
                _ => None,
            }
            .ok_or_else(|| {
                self.expected("a type (account, asset, monetary, number, portion, string) or `}`")
            })?;
            self.advance();
            let span = self.span();
            let Token::Var(name) = self.peek().clone() else {
                return Err(self.expected("a variable name like `$amount`"));
            };
            self.advance();
            if self.vars.insert(name.clone(), ty).is_some() {
                return Err(ParseError::new(
                    span,
                    format!("variable `${name}` is already declared"),
                ));
            }
            vars.push(VarDecl { name, ty });
        }
        self.advance();
        Ok(vars)
    }

    fn statement(&mut self) -> Result<Statement, ParseError> {
        if self.at_word("send") {
            self.advance();
            return Ok(Statement::Send(self.send()?));
        }
        if self.at_word("set_tx_meta") {
            self.advance();
            return self.set_tx_meta();
        }
        Err(self.expected("`send` or `set_tx_meta`"))
    }

    fn send(&mut self) -> Result<Send, ParseError> {
        let amount = self.monetary()?;
        self.symbol('(')?;
        self.word("source")?;
        self.symbol('=')?;
        let source = self.source()?;
        self.word("destination")?;
        self.symbol('=')?;
        let destination = self.destination()?;
        self.symbol(')')?;
        Ok(Send {
            amount,
            source,
            destination,
        })
    }

    fn set_tx_meta(&mut self) -> Result<Statement, ParseError> {
        self.symbol('(')?;
        let Token::Text(key) = self.peek().clone() else {
            return Err(self.expected("a metadata key string"));
        };
        self.advance();
        self.symbol(',')?;
        let value = match self.peek().clone() {
            Token::Text(text) => MetaValue::Text(text),
            Token::Var(name) => {
                self.declared(&name)?;
                MetaValue::Var(name)
            }
            _ => return Err(self.expected("a string or a variable")),
        };
        self.advance();
        self.symbol(')')?;
        Ok(Statement::SetTxMeta { key, value })
    }

    fn source(&mut self) -> Result<Vec<SourceItem>, ParseError> {
        if !self.at_symbol('{') {
            return Ok(vec![self.source_item()?]);
        }
        self.advance();
        let mut items = vec![self.source_item()?];
        while !self.at_symbol('}') {
            if !matches!(self.peek(), Token::Account(_) | Token::Var(_)) {
                return Err(self.expected("`max`, `allowing`, an account or `}`"));
            }
            items.push(self.source_item()?);
        }
        self.advance();
        Ok(items)
    }

    fn source_item(&mut self) -> Result<SourceItem, ParseError> {
        let account = self.account()?;
        let max = if self.at_word("max") {
            self.advance();
            Some(self.monetary()?)
        } else {
            None
        };
        let overdraft = if self.at_word("allowing") {
            self.advance();
            self.word("overdraft")?;
            self.word("up")?;
            self.word("to")?;
            Some(self.monetary()?)
        } else {
            None
        };
        Ok(SourceItem {
            account,
            max,
            overdraft,
        })
    }

    fn destination(&mut self) -> Result<Vec<DestinationItem>, ParseError> {
        if !self.at_symbol('{') {
            return Ok(vec![DestinationItem {
                share: Share::Remaining,
                account: self.account()?,
            }]);
        }
        self.advance();
        let mut items: Vec<DestinationItem> = Vec::new();
        loop {
            let span = self.span();
            if self.at_symbol('}') && !items.is_empty() {
                self.advance();
                return Ok(items);
            }
            if items.iter().any(|item| item.share == Share::Remaining) {
                return Err(ParseError::new(
                    span,
                    "`remaining` must be the last destination",
                ));
            }
            let share = self.share()?;
            self.word("to")?;
            let account = self.account()?;
            items.push(DestinationItem { share, account });
        }
    }

    fn share(&mut self) -> Result<Share, ParseError> {
        let span = self.span();
        match self.peek().clone() {
            Token::Word(word) if word == "remaining" => {
                self.advance();
                Ok(Share::Remaining)
            }
            Token::Var(name) => {
                self.typed(&name, VarType::Portion)?;
                self.advance();
                Ok(Share::Portion(Portion::Var(name)))
            }
            Token::Number(digits) => {
                self.advance();
                let ratio = if self.at_symbol('%') {
                    self.advance();
                    Ratio::percent(&digits)
                } else if self.at_symbol('/') {
                    self.advance();
                    let Token::Number(below) = self.peek().clone() else {
                        return Err(self.expected("a denominator"));
                    };
                    self.advance();
                    Ratio::parse(&format!("{digits}/{below}"))
                } else {
                    return Err(self.expected("`%` or `/`"));
                };
                let ratio = ratio.map_err(|error| ParseError::new(span, error.to_string()))?;
                Ok(Share::Portion(Portion::Literal(ratio)))
            }
            _ => Err(self.expected("a portion (`10%`, `1/3`, a variable) or `remaining`")),
        }
    }

    fn monetary(&mut self) -> Result<Monetary, ParseError> {
        match self.peek().clone() {
            Token::Var(name) => {
                self.typed(&name, VarType::Monetary)?;
                self.advance();
                Ok(Monetary::Var(name))
            }
            Token::Symbol('[') => {
                self.advance();
                let asset = match self.peek().clone() {
                    Token::Asset(asset) => Asset::Literal(asset),
                    Token::Var(name) => {
                        self.typed(&name, VarType::Asset)?;
                        Asset::Var(name)
                    }
                    _ => return Err(self.expected("an asset like `USDC@ethereum/6`")),
                };
                self.advance();
                let span = self.span();
                let amount = match self.peek().clone() {
                    Token::Number(digits) => Amount::Literal(integer(&digits, span)?),
                    Token::Var(name) => {
                        self.typed(&name, VarType::Number)?;
                        Amount::Var(name)
                    }
                    _ => return Err(self.expected("an amount in base units")),
                };
                self.advance();
                self.symbol(']')?;
                Ok(Monetary::Literal { asset, amount })
            }
            _ => Err(self.expected("an amount like `[USDC@ethereum/6 100]` or a variable")),
        }
    }

    fn account(&mut self) -> Result<Account, ParseError> {
        match self.peek().clone() {
            Token::Account(segments) => {
                for segment in &segments {
                    if let Segment::Var(name) = segment {
                        let ty = self.declared(name)?;
                        if !matches!(ty, VarType::Account | VarType::String | VarType::Number) {
                            return Err(ParseError::new(
                                self.span(),
                                format!(
                                    "variable `${name}` has type {}; only account, string and number variables can be part of an account",
                                    ty.keyword()
                                ),
                            ));
                        }
                    }
                }
                self.advance();
                Ok(Account::Template(segments))
            }
            Token::Var(name) => {
                self.typed(&name, VarType::Account)?;
                self.advance();
                Ok(Account::Var(name))
            }
            _ => Err(self.expected("an account like `@users:42:wallet` or a variable")),
        }
    }

    /// The type of a declared variable, or an error at the current token.
    fn declared(&self, name: &str) -> Result<VarType, ParseError> {
        self.vars.get(name).copied().ok_or_else(|| {
            ParseError::new(
                self.span(),
                format!("variable `${name}` is not declared in `vars`"),
            )
        })
    }

    /// Checks that `name` is declared with type `expected`.
    fn typed(&self, name: &str, expected: VarType) -> Result<(), ParseError> {
        let ty = self.declared(name)?;
        if ty != expected {
            return Err(ParseError::new(
                self.span(),
                format!(
                    "variable `${name}` has type {}, but {} is expected here",
                    ty.keyword(),
                    expected.keyword()
                ),
            ));
        }
        Ok(())
    }
}

fn integer(digits: &str, span: Span) -> Result<u128, ParseError> {
    if digits.contains('.') {
        return Err(ParseError::new(
            span,
            format!("`{digits}` must be a whole number"),
        ));
    }
    digits
        .parse()
        .map_err(|_| ParseError::new(span, format!("`{digits}` is too large")))
}
