use ast::ApiFile;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) fn render(ast: &ApiFile) -> String {
    let name = package_name(ast);
    format!(
        r#"# Code scaffolded by rust-zero. Safe to edit.

[package]
name = "{name}"
version = "0.1.0"
edition = "2021"
rust-version = "1.89"

[dependencies]
actix-web = "4"
serde = {{ version = "1", features = ["derive"] }}
rust-zero-core = "{VERSION}"
rust-zero-rest = "{VERSION}"
"#
    )
}

fn package_name(ast: &ApiFile) -> String {
    let raw = ast
        .services
        .first()
        .map(|service| service.name.as_str())
        .unwrap_or("app");
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if matches!(ch, '_' | '-') && !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "app".into()
    } else {
        out
    }
}
