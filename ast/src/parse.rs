//! Tokenize then parse a `.api` file.

use crate::ast::ApiFile;
use crate::lexer::{lex, LexError, Token};
use lalrpop_util::ParseError as LalrpopError;
use std::fmt;

#[allow(clippy::all, dead_code, unused_imports)]
mod grammar {
    include!(concat!(env!("OUT_DIR"), "/grammar.rs"));
}

pub(crate) enum TopItem {
    Type(crate::ast::TypeDef),
    Service(crate::ast::Service),
}

/// Parse error with a source span when available.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    Lex(#[from] LexError),
    #[error("{0}")]
    Grammar(String),
}

impl ParseError {
    fn from_lalrpop(err: LalrpopError<usize, Token, &'static str>) -> Self {
        Self::Grammar(format_lalrpop(err))
    }
}

fn format_lalrpop(err: LalrpopError<usize, Token, &'static str>) -> String {
    match err {
        LalrpopError::InvalidToken { location } => {
            format!("invalid token at byte {location}")
        }
        LalrpopError::UnrecognizedEof { location, expected } => {
            format!(
                "unexpected end of file at byte {location}, expected {}",
                expected.join(", ")
            )
        }
        LalrpopError::UnrecognizedToken { token, expected } => {
            format!(
                "unexpected {} {:?} at {:?}, expected {}",
                token.1.kind,
                token.1.text,
                token.1.span,
                expected.join(", ")
            )
        }
        LalrpopError::ExtraToken { token } => {
            format!("extra token {} {:?} at {:?}", token.1.kind, token.1.text, token.1.span)
        }
        LalrpopError::User { error } => error.to_string(),
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind, self.text)
    }
}

/// Parse a complete `.api` source string.
pub fn parse(source: &str) -> Result<ApiFile, ParseError> {
    let tokens = lex(source)?;
    let lexer = tokens
        .into_iter()
        .map(|token| Ok((token.span.start, token.clone(), token.span.end)));
    grammar::ApiFileParser::new()
        .parse(source, lexer)
        .map_err(ParseError::from_lalrpop)
}

pub(crate) fn unquote(raw: &str) -> String {
    if raw.len() >= 2 {
        let bytes = raw.as_bytes();
        if bytes[0] == b'"' && bytes[raw.len() - 1] == b'"' {
            return raw[1..raw.len() - 1].to_string();
        }
    }
    raw.to_string()
}

pub(crate) fn token_text(token: &Token) -> String {
    token.text.clone()
}
