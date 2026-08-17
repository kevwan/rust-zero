//! Semantic AST for a rust-zero `.api` file.

/// Parsed `.api` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiFile {
    pub syntax: Syntax,
    pub imports: Vec<String>,
    pub info: Option<InfoBlock>,
    pub types: Vec<TypeDef>,
    pub services: Vec<Service>,
}

/// `syntax = "v1"`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Syntax {
    pub version: String,
}

/// `info ( key: value )`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoBlock {
    pub items: Vec<InfoItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoItem {
    pub key: String,
    pub value: String,
}

/// `struct Name { fields }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDef {
    pub name: String,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub attrs: Vec<FieldAttr>,
    pub name: String,
    pub ty: TypeExpr,
}

/// `#[path]`, `#[path("id")]`, `#[json("user_id")]`, `#[validate(required, length(1, 32))]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldAttr {
    pub name: String,
    pub args: Vec<AttrArg>,
}

impl FieldAttr {
    /// Single string or ident argument, like `#[json("user_id")]` or `#[path(id)]`.
    pub fn value(&self) -> Option<&str> {
        match self.args.as_slice() {
            [AttrArg::String(value) | AttrArg::Ident(value)] => Some(value),
            _ => None,
        }
    }
}

/// One argument of a field attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrArg {
    Ident(String),
    String(String),
    Int(String),
    Nested(FieldAttr),
}

/// Type algebra: `Name | [T] | {K: V} | T?`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeExpr {
    Named(String),
    List(Box<TypeExpr>),
    Map {
        key: Box<TypeExpr>,
        value: Box<TypeExpr>,
    },
    Optional(Box<TypeExpr>),
}

/// `#[server(...)] service name { routes }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub server: Option<AtServer>,
    pub name: String,
    pub routes: Vec<Route>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtServer {
    pub items: Vec<InfoItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtDoc {
    pub items: Vec<InfoItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Connect,
    Options,
    Trace,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Head => "head",
            Self::Post => "post",
            Self::Put => "put",
            Self::Patch => "patch",
            Self::Delete => "delete",
            Self::Connect => "connect",
            Self::Options => "options",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub doc: Option<AtDoc>,
    pub handler: Option<String>,
    pub method: HttpMethod,
    pub path: String,
    pub request: Option<TypeExpr>,
    pub returns: Option<TypeExpr>,
}
