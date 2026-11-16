//! Splits script text into tokens with their positions. Accounts and assets are single tokens,
//! so the parser sees `@users:{$buyer}:wallet` and `USDC@ethereum/6` already structured.

use ledger_core::AssetId;

use crate::ast::{Segment, Span};
use crate::error::ParseError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A keyword or other lowercase word: `send`, `max`, `account`, …
    Word(String),
    /// `$name`
    Var(String),
    /// `@segment:{$var}:segment`
    Account(Vec<Segment>),
    /// `SYMBOL@network/decimals`
    Asset(AssetId),
    /// Digits, optionally with a fractional part (only valid before `%`).
    Number(String),
    /// A double-quoted string, unescaped.
    Text(String),
    /// One of `{ } ( ) [ ] = % / ,`
    Symbol(char),
    End,
}

impl Token {
    /// How the token is named in "found …" error messages.
    pub fn describe(&self) -> String {
        match self {
            Token::Word(word) => format!("`{word}`"),
            Token::Var(name) => format!("`${name}`"),
            Token::Account(_) => "an account".to_owned(),
            Token::Asset(asset) => format!("asset `{asset}`"),
            Token::Number(digits) => format!("`{digits}`"),
            Token::Text(_) => "a string".to_owned(),
            Token::Symbol(symbol) => format!("`{symbol}`"),
            Token::End => "the end of the script".to_owned(),
        }
    }
}

const SYMBOLS: &str = "{}()[]=%/,";

pub fn tokenize(source: &str) -> Result<Vec<(Token, Span)>, ParseError> {
    let mut lexer = Lexer {
        chars: source.chars().collect(),
        pos: 0,
        line: 1,
        column: 1,
    };
    let mut tokens = Vec::new();
    loop {
        lexer.skip_blanks_and_comments();
        let span = lexer.span();
        let Some(c) = lexer.peek() else {
            tokens.push((Token::End, span));
            return Ok(tokens);
        };
        let token = match c {
            'a'..='z' | '_' => Token::Word(lexer.take_while(is_word_char)),
            '$' => {
                lexer.bump();
                Token::Var(lexer.variable_name(span)?)
            }
            '@' => {
                lexer.bump();
                Token::Account(lexer.account()?)
            }
            'A'..='Z' => {
                let raw = lexer.take_while(|c| c.is_ascii_alphanumeric() || "@/_".contains(c));
                let asset = raw.parse::<AssetId>().map_err(|error| {
                    ParseError::new(span, format!("invalid asset `{raw}`: {error}"))
                })?;
                Token::Asset(asset)
            }
            '0'..='9' => Token::Number(lexer.number()),
            '"' => Token::Text(lexer.text(span)?),
            c if SYMBOLS.contains(c) => {
                lexer.bump();
                Token::Symbol(c)
            }
            other => {
                return Err(ParseError::new(
                    span,
                    format!("unexpected character `{other}`"),
                ));
            }
        };
        tokens.push((token, span));
    }
}

fn is_word_char(c: char) -> bool {
    matches!(c, 'a'..='z' | '0'..='9' | '_')
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: u32,
    column: u32,
}

impl Lexer {
    fn span(&self) -> Span {
        Span {
            line: self.line,
            column: self.column,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_second(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn take_while(&mut self, keep: impl Fn(char) -> bool) -> String {
        let mut taken = String::new();
        while let Some(c) = self.peek().filter(|&c| keep(c)) {
            taken.push(c);
            self.bump();
        }
        taken
    }

    fn skip_blanks_and_comments(&mut self) {
        loop {
            match (self.peek(), self.peek_second()) {
                (Some(c), _) if c.is_whitespace() => {
                    self.bump();
                }
                (Some('/'), Some('/')) => {
                    self.take_while(|c| c != '\n');
                }
                _ => return,
            }
        }
    }

    /// A variable name after `$`: `[a-z_][a-z0-9_]*`.
    fn variable_name(&mut self, span: Span) -> Result<String, ParseError> {
        let name = self.take_while(is_word_char);
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(ParseError::new(
                span,
                "expected a variable name after `$` (a-z, 0-9 and _, not starting with a digit)",
            ));
        }
        Ok(name)
    }

    /// The segments of an address after `@`: lowercase words or `{$var}`, separated by `:`.
    /// The full address is validated once variables are bound.
    fn account(&mut self) -> Result<Vec<Segment>, ParseError> {
        let mut segments = Vec::new();
        loop {
            let span = self.span();
            let segment = if self.peek() == Some('{') {
                self.bump();
                if self.bump() != Some('$') {
                    return Err(ParseError::new(span, "expected `{$name}` in an account"));
                }
                let name = self.variable_name(span)?;
                if self.bump() != Some('}') {
                    return Err(ParseError::new(span, "expected `}` closing `{$name}`"));
                }
                Segment::Var(name)
            } else {
                let text = self.take_while(is_word_char);
                if text.is_empty() {
                    return Err(ParseError::new(
                        span,
                        "expected an account segment (a-z, 0-9, _ or `{$name}`)",
                    ));
                }
                Segment::Text(text)
            };
            segments.push(segment);
            match self.peek() {
                Some(':') => {
                    self.bump();
                }
                Some(c) if is_word_char(c) || c == '{' || c.is_ascii_uppercase() => {
                    return Err(ParseError::new(
                        self.span(),
                        format!("unexpected `{c}` in an account; segments are separated by `:`"),
                    ));
                }
                _ => break,
            }
        }
        Ok(segments)
    }

    fn number(&mut self) -> String {
        let mut digits = self.take_while(|c| c.is_ascii_digit());
        if self.peek() == Some('.') && self.peek_second().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
            digits.push('.');
            digits.push_str(&self.take_while(|c| c.is_ascii_digit()));
        }
        digits
    }

    /// A `"…"` string with `\"`, `\\` and `\n` escapes; no raw line breaks.
    fn text(&mut self, start: Span) -> Result<String, ParseError> {
        self.bump();
        let mut text = String::new();
        loop {
            let span = self.span();
            match self.bump() {
                Some('"') => return Ok(text),
                Some('\\') => match self.bump() {
                    Some('"') => text.push('"'),
                    Some('\\') => text.push('\\'),
                    Some('n') => text.push('\n'),
                    _ => {
                        return Err(ParseError::new(
                            span,
                            "unknown escape; use \\\", \\\\ or \\n",
                        ));
                    }
                },
                Some('\n') | None => {
                    return Err(ParseError::new(start, "unterminated string"));
                }
                Some(c) => text.push(c),
            }
        }
    }
}
