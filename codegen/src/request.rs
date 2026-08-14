use crate::util::rust_ident;
use ast::{ApiFile, Field, TypeDef, TypeExpr};
use syn::Ident;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Location {
    Path,
    Query,
    Header,
    Form,
    Json,
}

#[derive(Debug, Clone)]
pub(crate) struct RequestViews<'a> {
    pub original: &'a TypeDef,
    pub path: Vec<&'a Field>,
    pub query: Vec<&'a Field>,
    pub headers: Vec<&'a Field>,
    pub form: Vec<&'a Field>,
    pub json: Vec<&'a Field>,
}

impl<'a> RequestViews<'a> {
    pub(crate) fn is_json_only(&self) -> bool {
        self.path.is_empty()
            && self.query.is_empty()
            && self.headers.is_empty()
            && self.form.is_empty()
    }

    pub(crate) fn view_ident(&self, suffix: &str) -> Ident {
        rust_ident(&format!("{}{suffix}", self.original.name))
    }
}

pub(crate) fn field_location(field: &Field) -> Location {
    for attr in &field.attrs {
        match attr.name.as_str() {
            "path" => return Location::Path,
            "query" => return Location::Query,
            "header" => return Location::Header,
            "form" => return Location::Form,
            _ => {}
        }
    }
    Location::Json
}

pub(crate) fn attr_value<'a>(field: &'a Field, name: &str) -> Option<&'a str> {
    field.attrs.iter().find_map(|attr| {
        if attr.name == name {
            attr.value.as_deref()
        } else {
            None
        }
    })
}

pub(crate) fn request_views<'a>(ast: &'a ApiFile, name: &str) -> Option<RequestViews<'a>> {
    let original = ast.types.iter().find(|def| def.name == name)?;
    let mut views = RequestViews {
        original,
        path: Vec::new(),
        query: Vec::new(),
        headers: Vec::new(),
        form: Vec::new(),
        json: Vec::new(),
    };
    for field in &original.fields {
        match field_location(field) {
            Location::Path => views.path.push(field),
            Location::Query => views.query.push(field),
            Location::Header => views.headers.push(field),
            Location::Form => views.form.push(field),
            Location::Json => views.json.push(field),
        }
    }
    Some(views)
}

pub(crate) fn request_type_name(ty: &TypeExpr) -> Option<&str> {
    match ty {
        TypeExpr::Named(name) => Some(name.as_str()),
        _ => None,
    }
}
