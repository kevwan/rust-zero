//! rust-zero `.api` DSL lexer and parser.

mod ast;
mod lexer;
mod parse;

pub use ast::{
    ApiFile, AtDoc, AtServer, Field, FieldAttr, HttpMethod, InfoBlock, InfoItem, Route, Service,
    Syntax, TypeDef, TypeExpr,
};
pub use lexer::{lex, LexError, Token, TokenKind};
pub use parse::{parse, ParseError};
