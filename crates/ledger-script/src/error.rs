//! Parse errors carry the position and say what was expected and what was found (FR-60).

use crate::ast::Span;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {}, column {}: {message}", span.line, span.column)]
pub struct ParseError {
    pub span: Span,
    pub message: String,
}

impl ParseError {
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}
