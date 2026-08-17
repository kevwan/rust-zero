use ast::{AttrArg, Field, TypeExpr};
use quote::quote;

#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    #[error("unknown validate rule `{rule}` on {type_name}.{field}")]
    UnknownValidate {
        type_name: String,
        field: String,
        rule: String,
    },
    #[error("invalid validate rule `{rule}` on {type_name}.{field}: {reason}")]
    InvalidValidate {
        type_name: String,
        field: String,
        rule: String,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum ValidateRule {
    Required,
    Length { min: u64, max: u64 },
    Range { min: String, max: String },
    OneOf { values: Vec<AttrArg> },
}

pub(crate) fn field_rules(
    type_name: &str,
    field: &Field,
) -> Result<Vec<ValidateRule>, GenerateError> {
    let mut rules = Vec::new();
    for attr in &field.attrs {
        if attr.name != "validate" {
            continue;
        }
        if attr.args.is_empty() {
            return Err(invalid(
                type_name,
                &field.name,
                "validate",
                "expected at least one rule",
            ));
        }
        for arg in &attr.args {
            rules.push(parse_rule(type_name, field, arg)?);
        }
    }
    Ok(rules)
}

pub(crate) fn has_validate_rules<'a>(fields: impl IntoIterator<Item = &'a Field>) -> bool {
    fields
        .into_iter()
        .any(|field| field.attrs.iter().any(|attr| attr.name == "validate"))
}

pub(crate) fn render_checks(
    type_name: &str,
    field: &Field,
) -> Result<Vec<proc_macro2::TokenStream>, GenerateError> {
    let fname = crate::util::rust_ident(&field.name);
    let field_name = field.name.as_str();
    field_rules(type_name, field)?
        .into_iter()
        .map(|rule| render_check(type_name, field, &fname, field_name, rule))
        .collect()
}

fn parse_rule(
    type_name: &str,
    field: &Field,
    arg: &AttrArg,
) -> Result<ValidateRule, GenerateError> {
    match arg {
        AttrArg::Ident(name) | AttrArg::String(name) if name == "required" => {
            Ok(ValidateRule::Required)
        }
        AttrArg::Ident(name) | AttrArg::String(name) => Err(unknown(type_name, &field.name, name)),
        AttrArg::Nested(inner) => match inner.name.as_str() {
            "required" => {
                if inner.args.is_empty() {
                    Ok(ValidateRule::Required)
                } else {
                    Err(invalid(
                        type_name,
                        &field.name,
                        "required",
                        "required takes no arguments",
                    ))
                }
            }
            "length" => {
                let (min, max) = two_ints(type_name, &field.name, "length", &inner.args)?;
                Ok(ValidateRule::Length { min, max })
            }
            "range" => {
                let (min, max) = two_int_tokens(type_name, &field.name, "range", &inner.args)?;
                Ok(ValidateRule::Range { min, max })
            }
            "one_of" => {
                if inner.args.is_empty() {
                    return Err(invalid(
                        type_name,
                        &field.name,
                        "one_of",
                        "expected at least one value",
                    ));
                }
                if !inner.args.iter().all(|arg| {
                    matches!(arg, AttrArg::String(_) | AttrArg::Int(_) | AttrArg::Ident(_))
                }) {
                    return Err(invalid(
                        type_name,
                        &field.name,
                        "one_of",
                        "values must be strings, ints, or idents",
                    ));
                }
                Ok(ValidateRule::OneOf {
                    values: inner.args.clone(),
                })
            }
            other => Err(unknown(type_name, &field.name, other)),
        },
        AttrArg::Int(value) => Err(unknown(type_name, &field.name, value)),
    }
}

fn render_check(
    type_name: &str,
    field: &Field,
    fname: &syn::Ident,
    field_name: &str,
    rule: ValidateRule,
) -> Result<proc_macro2::TokenStream, GenerateError> {
    match rule {
        ValidateRule::Required => {
            ensure_string(type_name, field, "required")?;
            Ok(quote! {
                .required(#field_name, &self.#fname)
            })
        }
        ValidateRule::Length { min, max } => {
            ensure_string(type_name, field, "length")?;
            let min = lit_int(min);
            let max = lit_int(max);
            Ok(quote! {
                .length(#field_name, &self.#fname, #min..=#max)
            })
        }
        ValidateRule::Range { min, max } => {
            ensure_number(type_name, field, "range")?;
            let min = syn::parse_str::<syn::LitInt>(&min).expect("range min");
            let max = syn::parse_str::<syn::LitInt>(&max).expect("range max");
            Ok(quote! {
                .range(#field_name, self.#fname, #min..=#max)
            })
        }
        ValidateRule::OneOf { values } => {
            let allowed = match named_type(&field.ty) {
                Some("String") => values
                    .iter()
                    .map(|value| match value {
                        AttrArg::String(text) | AttrArg::Ident(text) => Ok(quote!(#text)),
                        AttrArg::Int(text) => Err(invalid(
                            type_name,
                            &field.name,
                            "one_of",
                            &format!("string field cannot use int value {text}"),
                        )),
                        AttrArg::Nested(_) => unreachable!("filtered earlier"),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(name) if is_number(name) => values
                    .iter()
                    .map(|value| match value {
                        AttrArg::Int(text) => {
                            let lit = syn::parse_str::<syn::LitInt>(text).expect("one_of int");
                            Ok(quote!(#lit))
                        }
                        _ => Err(invalid(
                            type_name,
                            &field.name,
                            "one_of",
                            "numeric field requires int values",
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(name) => {
                    return Err(invalid(
                        type_name,
                        &field.name,
                        "one_of",
                        &format!("unsupported type {name}"),
                    ))
                }
                None => {
                    return Err(invalid(
                        type_name,
                        &field.name,
                        "one_of",
                        "only named types can use one_of",
                    ))
                }
            };
            let value = if named_type(&field.ty) == Some("String") {
                quote!(self.#fname.as_str())
            } else {
                quote!(&self.#fname)
            };
            Ok(quote! {
                .one_of(#field_name, #value, &[#(#allowed),*])
            })
        }
    }
}

fn two_ints(
    type_name: &str,
    field: &str,
    rule: &str,
    args: &[AttrArg],
) -> Result<(u64, u64), GenerateError> {
    match args {
        [AttrArg::Int(min), AttrArg::Int(max)] => {
            let min = min.parse::<u64>().map_err(|_| {
                invalid(type_name, field, rule, &format!("invalid min {min}"))
            })?;
            let max = max.parse::<u64>().map_err(|_| {
                invalid(type_name, field, rule, &format!("invalid max {max}"))
            })?;
            if min > max {
                return Err(invalid(
                    type_name,
                    field,
                    rule,
                    "min must be <= max",
                ));
            }
            Ok((min, max))
        }
        _ => Err(invalid(
            type_name,
            field,
            rule,
            "expected two integer arguments",
        )),
    }
}

fn two_int_tokens(
    type_name: &str,
    field: &str,
    rule: &str,
    args: &[AttrArg],
) -> Result<(String, String), GenerateError> {
    match args {
        [AttrArg::Int(min), AttrArg::Int(max)] => Ok((min.clone(), max.clone())),
        _ => Err(invalid(
            type_name,
            field,
            rule,
            "expected two integer arguments",
        )),
    }
}

fn ensure_string(type_name: &str, field: &Field, rule: &str) -> Result<(), GenerateError> {
    match named_type(&field.ty) {
        Some("String") => Ok(()),
        Some(name) => Err(invalid(
            type_name,
            &field.name,
            rule,
            &format!("only String supports {rule}, got {name}"),
        )),
        None => Err(invalid(
            type_name,
            &field.name,
            rule,
            &format!("only String supports {rule}"),
        )),
    }
}

fn ensure_number(type_name: &str, field: &Field, rule: &str) -> Result<(), GenerateError> {
    match named_type(&field.ty) {
        Some(name) if is_number(name) => Ok(()),
        Some(name) => Err(invalid(
            type_name,
            &field.name,
            rule,
            &format!("only numeric types support {rule}, got {name}"),
        )),
        None => Err(invalid(
            type_name,
            &field.name,
            rule,
            &format!("only numeric types support {rule}"),
        )),
    }
}

fn named_type(ty: &TypeExpr) -> Option<&str> {
    match ty {
        TypeExpr::Named(name) => Some(name.as_str()),
        _ => None,
    }
}

fn is_number(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
    )
}

fn lit_int(value: u64) -> syn::LitInt {
    syn::parse_str(&value.to_string()).expect("int literal")
}

fn unknown(type_name: &str, field: &str, rule: &str) -> GenerateError {
    GenerateError::UnknownValidate {
        type_name: type_name.to_owned(),
        field: field.to_owned(),
        rule: rule.to_owned(),
    }
}

fn invalid(type_name: &str, field: &str, rule: &str, reason: &str) -> GenerateError {
    GenerateError::InvalidValidate {
        type_name: type_name.to_owned(),
        field: field.to_owned(),
        rule: rule.to_owned(),
        reason: reason.to_owned(),
    }
}
