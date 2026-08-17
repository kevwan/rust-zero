use ast::{Field, HttpMethod, Route, TypeExpr};
use proc_macro2::{Span, TokenStream};
use quote::quote;
use std::io::Write;
use std::process::{Command, Stdio};
use syn::{Ident, Type};

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where", "while",
];

pub(crate) fn pretty(banner: &str, tokens: TokenStream) -> String {
    let file: syn::File = syn::parse2(tokens.clone())
        .unwrap_or_else(|err| panic!("generated invalid Rust: {err}\n{tokens}"));
    let source = format!("{banner}{}", prettyplease::unparse(&file));
    let formatted = rustfmt(&source).unwrap_or(source);
    space_items(&formatted)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Use,
    Mod,
    Attr,
    Item,
}

fn item_kind(line: &str) -> Option<ItemKind> {
    if line.starts_with("use ") {
        Some(ItemKind::Use)
    } else if line.starts_with("mod ") {
        Some(ItemKind::Mod)
    } else if line.starts_with("#[") {
        Some(ItemKind::Attr)
    } else if line.starts_with("pub ") || line.starts_with("fn ") || line.starts_with("impl ") {
        Some(ItemKind::Item)
    } else {
        None
    }
}

fn space_items(source: &str) -> String {
    let mut out = String::new();
    let mut prev = None;
    for line in source.lines() {
        let kind = item_kind(line);
        let blank = match (prev, kind) {
            (Some(ItemKind::Use), Some(ItemKind::Use))
            | (Some(ItemKind::Mod), Some(ItemKind::Mod))
            | (Some(ItemKind::Attr), Some(ItemKind::Attr))
            | (Some(ItemKind::Attr), Some(ItemKind::Item)) => false,
            (Some(_), Some(_)) => true,
            _ => false,
        };
        if blank {
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
        if line.is_empty() {
            prev = None;
        } else if !line.starts_with(' ') && !line.starts_with('\t') {
            if kind.is_some() {
                prev = kind;
            }
        }
    }
    out
}

fn rustfmt(source: &str) -> Option<String> {
    let mut child = Command::new("rustfmt")
        .args(["--emit", "stdout", "--quiet", "--edition", "2021"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(source.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

pub(crate) fn rust_ident(name: &str) -> Ident {
    if KEYWORDS.contains(&name) {
        Ident::new_raw(name, Span::call_site())
    } else {
        Ident::new(name, Span::call_site())
    }
}

pub(crate) fn rust_type(ty: &TypeExpr) -> Type {
    match ty {
        TypeExpr::Named(name) => {
            let ident = rust_ident(name);
            syn::parse_quote!(#ident)
        }
        TypeExpr::List(inner) => {
            let inner = rust_type(inner);
            syn::parse_quote!(Vec<#inner>)
        }
        TypeExpr::Map { key, value } => {
            let key = rust_type(key);
            let value = rust_type(value);
            syn::parse_quote!(HashMap<#key, #value>)
        }
        TypeExpr::Optional(inner) => {
            let inner = rust_type(inner);
            syn::parse_quote!(Option<#inner>)
        }
    }
}

pub(crate) fn type_needs_hashmap(ty: &TypeExpr) -> bool {
    match ty {
        TypeExpr::Named(_) => false,
        TypeExpr::List(inner) | TypeExpr::Optional(inner) => type_needs_hashmap(inner),
        TypeExpr::Map { .. } => true,
    }
}

pub(crate) fn json_rename(field: &Field) -> Option<&str> {
    field.attrs.iter().find_map(|attr| {
        if attr.name == "json" {
            attr.value()
        } else {
            None
        }
    })
}

pub(crate) fn handler_name(route: &Route) -> Ident {
    rust_ident(&handler_fn_name(route))
}

pub(crate) fn handler_fn_name(route: &Route) -> String {
    let raw = if let Some(name) = &route.handler {
        name.clone()
    } else {
        handler_name_from_path(&route.path)
    };
    to_snake(&raw)
}

pub(crate) fn service_mod_name(name: &str) -> Ident {
    let mut base = to_snake(name);
    if base.is_empty() {
        base = "service".into();
    }
    if base.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        base = format!("service_{base}");
    }
    rust_ident(&base)
}

pub(crate) fn to_snake(name: &str) -> String {
    let mut out = String::new();
    for (idx, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if idx > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        "handler".into()
    } else {
        out
    }
}

pub(crate) fn handler_name_from_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return "root".into();
    }
    let mut out = String::new();
    for ch in trimmed.chars() {
        match ch {
            '/' | '-' | ':' => {
                if !out.is_empty() && !out.ends_with('_') {
                    out.push('_');
                }
            }
            c if c.is_ascii_alphanumeric() || c == '_' => out.push(c),
            _ => {}
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        "root".into()
    } else if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("r_{out}")
    } else {
        out
    }
}

pub(crate) fn actix_path(path: &str) -> String {
    let mut out = String::new();
    let mut chars = path.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == ':' {
            out.push('{');
            while let Some(&next) = chars.peek() {
                if next.is_ascii_alphanumeric() || next == '_' {
                    out.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push('}');
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        "/".into()
    } else {
        out
    }
}

pub(crate) fn join_path(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    if path == "/" {
        if prefix.is_empty() {
            "/".into()
        } else {
            prefix.to_string()
        }
    } else if prefix.is_empty() {
        path.to_string()
    } else {
        format!("{prefix}{path}")
    }
}

pub(crate) fn method_name(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Head => "HEAD",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Connect => "CONNECT",
        HttpMethod::Options => "OPTIONS",
        HttpMethod::Trace => "TRACE",
    }
}

pub(crate) fn normalize_prefix(prefix: &str) -> String {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        String::new()
    } else if prefix.starts_with('/') {
        prefix.trim_end_matches('/').to_string()
    } else {
        format!("/{}", prefix.trim_end_matches('/'))
    }
}

pub(crate) fn duration_to_ms(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut total_ns: u128 = 0;
    let mut idx = 0;
    let bytes = raw.as_bytes();
    while idx < bytes.len() {
        let start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if start == idx {
            return None;
        }
        let amount: u128 = raw[start..idx].parse().ok()?;
        let unit_start = idx;
        while idx < bytes.len() && !bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        let unit = &raw[unit_start..idx];
        let factor = match unit {
            "ns" => 1,
            "us" | "µs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 3_600 * 1_000_000_000,
            _ => return None,
        };
        total_ns = total_ns.saturating_add(amount.saturating_mul(factor));
    }
    Some((total_ns / 1_000_000) as u64)
}

pub(crate) fn method_builder(method: HttpMethod) -> TokenStream {
    match method {
        HttpMethod::Get => quote!(web::get()),
        HttpMethod::Head => quote!(web::head()),
        HttpMethod::Post => quote!(web::post()),
        HttpMethod::Put => quote!(web::put()),
        HttpMethod::Patch => quote!(web::patch()),
        HttpMethod::Delete => quote!(web::delete()),
        HttpMethod::Connect => quote!(web::route().method(actix_web::http::Method::CONNECT)),
        HttpMethod::Options => quote!(web::route().method(actix_web::http::Method::OPTIONS)),
        HttpMethod::Trace => quote!(web::route().method(actix_web::http::Method::TRACE)),
    }
}

const BUILTIN_TYPES: &[&str] = &[
    "String", "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128",
    "isize", "u8", "u16", "u32", "u64", "u128", "usize",
];

pub(crate) fn collect_type_idents(ty: Option<&TypeExpr>, names: &mut Vec<Ident>) {
    match ty {
        Some(TypeExpr::Named(name)) => {
            if BUILTIN_TYPES.contains(&name.as_str()) {
                return;
            }
            let ident = rust_ident(name);
            if !names.iter().any(|existing| existing == &ident) {
                names.push(ident);
            }
        }
        Some(TypeExpr::List(inner) | TypeExpr::Optional(inner)) => {
            collect_type_idents(Some(inner), names);
        }
        Some(TypeExpr::Map { key, value }) => {
            collect_type_idents(Some(key), names);
            collect_type_idents(Some(value), names);
        }
        None => {}
    }
}
