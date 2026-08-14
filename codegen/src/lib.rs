//! rust-zero `.api` AST → rust-zero REST source.

mod handlers;
mod main_rs;
mod request;
mod routes;
mod types;
mod util;

use ast::ApiFile;

/// One generated source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    pub path: String,
    pub contents: String,
}

/// Generate rust-zero REST files from a parsed `.api` tree.
pub fn generate(ast: &ApiFile) -> Vec<GeneratedFile> {
    let mut files = vec![
        GeneratedFile {
            path: "src/main.rs".into(),
            contents: main_rs::render(),
        },
        GeneratedFile {
            path: "src/types.rs".into(),
            contents: types::render(ast),
        },
        GeneratedFile {
            path: "src/routes.rs".into(),
            contents: routes::render(ast),
        },
        GeneratedFile {
            path: "src/handlers/mod.rs".into(),
            contents: handlers::render_mod(ast),
        },
    ];
    files.extend(handlers::render_files(ast).into_iter().map(|(path, contents)| {
        GeneratedFile { path, contents }
    }));
    files
}
