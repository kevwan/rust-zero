//! Logos lexer for rust-zero `.api` text.

use logos::Logos;
use std::fmt;

/// Token kinds after comments and whitespace are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Syntax,
    Import,
    Info,
    Struct,
    Service,
    Ident,
    Int,
    Duration,
    String,
    Path,
    HashLBracket,
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Connect,
    Options,
    Trace,
    Arrow,
    Question,
    Sub,
    Star,
    Slash,
    Assign,
    LParen,
    LBracket,
    LBrace,
    Comma,
    Dot,
    RParen,
    RBrace,
    RBracket,
    Semicolon,
    Colon,
}

/// A lexed token with source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub text: String,
    pub span: std::ops::Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid token at {span:?}: {text}")]
pub struct LexError {
    pub span: std::ops::Range<usize>,
    pub text: String,
}

#[derive(Logos, Debug, Clone, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n]+")]
#[logos(skip r"//[^\n]*")]
#[logos(skip r"/\*([^*]|\*[^/])*\*/")]
enum RawToken {
    #[token("syntax")]
    Syntax,
    #[token("import")]
    Import,
    #[token("info")]
    Info,
    #[token("struct")]
    Struct,
    #[token("service")]
    Service,
    #[token("#[")]
    HashLBracket,
    #[token("get")]
    Get,
    #[token("head")]
    Head,
    #[token("post")]
    Post,
    #[token("put")]
    Put,
    #[token("patch")]
    Patch,
    #[token("delete")]
    Delete,
    #[token("connect")]
    Connect,
    #[token("options")]
    Options,
    #[token("trace")]
    Trace,
    #[token("->")]
    Arrow,
    #[token("?")]
    Question,
    #[token("-")]
    Sub,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("=")]
    Assign,
    #[token("(")]
    LParen,
    #[token("[")]
    LBracket,
    #[token("{")]
    LBrace,
    #[token(",")]
    Comma,
    #[token(".")]
    Dot,
    #[token(")")]
    RParen,
    #[token("}")]
    RBrace,
    #[token("]")]
    RBracket,
    #[token(";")]
    Semicolon,
    #[token(":")]
    Colon,
    #[regex(r"[0-9]+(?:ns|us|µs|ms|s|m|h)(?:[0-9]+(?:ns|us|µs|ms|s|m|h))*")]
    Duration,
    #[regex(r"[0-9]+")]
    Int,
    #[regex(r#""([^"\\]|\\.)*""#)]
    String,
    #[regex(r"/(?:[A-Za-z0-9._~:@!$&'*+,;=%\-]|:[A-Za-z_][A-Za-z0-9_]*)+(?:/(?:[A-Za-z0-9._~:@!$&'*+,;=%\-]|:[A-Za-z_][A-Za-z0-9_]*)+)*")]
    Path,
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Ident,
}

impl From<RawToken> for TokenKind {
    fn from(value: RawToken) -> Self {
        match value {
            RawToken::Syntax => Self::Syntax,
            RawToken::Import => Self::Import,
            RawToken::Info => Self::Info,
            RawToken::Struct => Self::Struct,
            RawToken::Service => Self::Service,
            RawToken::HashLBracket => Self::HashLBracket,
            RawToken::Get => Self::Get,
            RawToken::Head => Self::Head,
            RawToken::Post => Self::Post,
            RawToken::Put => Self::Put,
            RawToken::Patch => Self::Patch,
            RawToken::Delete => Self::Delete,
            RawToken::Connect => Self::Connect,
            RawToken::Options => Self::Options,
            RawToken::Trace => Self::Trace,
            RawToken::Arrow => Self::Arrow,
            RawToken::Question => Self::Question,
            RawToken::Sub => Self::Sub,
            RawToken::Star => Self::Star,
            RawToken::Slash => Self::Slash,
            RawToken::Assign => Self::Assign,
            RawToken::LParen => Self::LParen,
            RawToken::LBracket => Self::LBracket,
            RawToken::LBrace => Self::LBrace,
            RawToken::Comma => Self::Comma,
            RawToken::Dot => Self::Dot,
            RawToken::RParen => Self::RParen,
            RawToken::RBrace => Self::RBrace,
            RawToken::RBracket => Self::RBracket,
            RawToken::Semicolon => Self::Semicolon,
            RawToken::Colon => Self::Colon,
            RawToken::Duration => Self::Duration,
            RawToken::Int => Self::Int,
            RawToken::String => Self::String,
            RawToken::Path => Self::Path,
            RawToken::Ident => Self::Ident,
        }
    }
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Syntax => "syntax",
            Self::Import => "import",
            Self::Info => "info",
            Self::Struct => "struct",
            Self::Service => "service",
            Self::Ident => "ident",
            Self::Int => "int",
            Self::Duration => "duration",
            Self::String => "string",
            Self::Path => "path",
            Self::HashLBracket => "#[",
            Self::Get => "get",
            Self::Head => "head",
            Self::Post => "post",
            Self::Put => "put",
            Self::Patch => "patch",
            Self::Delete => "delete",
            Self::Connect => "connect",
            Self::Options => "options",
            Self::Trace => "trace",
            Self::Arrow => "->",
            Self::Question => "?",
            Self::Sub => "-",
            Self::Star => "*",
            Self::Slash => "/",
            Self::Assign => "=",
            Self::LParen => "(",
            Self::LBracket => "[",
            Self::LBrace => "{",
            Self::Comma => ",",
            Self::Dot => ".",
            Self::RParen => ")",
            Self::RBrace => "}",
            Self::RBracket => "]",
            Self::Semicolon => ";",
            Self::Colon => ":",
        })
    }
}

/// Lex `.api` source into parser tokens. Comments and whitespace are dropped.
pub fn lex(source: &str) -> Result<Vec<Token>, LexError> {
    let mut lexer = RawToken::lexer(source);
    let mut tokens = Vec::new();
    while let Some(item) = lexer.next() {
        match item {
            Ok(raw) => tokens.push(Token {
                kind: raw.into(),
                text: lexer.slice().to_string(),
                span: lexer.span(),
            }),
            Err(()) => {
                return Err(LexError {
                    span: lexer.span(),
                    text: lexer.slice().to_string(),
                });
            }
        }
    }
    Ok(tokens)
}
