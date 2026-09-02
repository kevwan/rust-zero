//! rust-zero `.api` DSL lexer and parser.

mod ast;
mod lexer;
mod load;
mod parse;

pub use ast::{
    ApiFile, AtDoc, AtServer, AttrArg, Field, FieldAttr, HttpMethod, InfoBlock, InfoItem, Route,
    Service,
    Syntax, TypeDef, TypeExpr,
};
pub use lexer::{lex, LexError, Token, TokenKind};
pub use load::{load, Bundle, LoadError};
pub use parse::{parse, ParseError};
