//! rust-zero `.api` AST → rust-zero REST source.

mod entry;
mod handlers;
mod manifest;
mod request;
mod routes;
mod types;
mod util;
mod validate;

use ast::ApiFile;

pub use validate::GenerateError;

/// One generated source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    pub path: String,
    pub contents: String,
}

/// Generate rust-zero REST files from a parsed `.api` tree.
pub fn generate(ast: &ApiFile) -> Result<Vec<GeneratedFile>, GenerateError> {
    let mut files = vec![
        GeneratedFile {
            path: "Cargo.toml".into(),
            contents: manifest::render(ast),
        },
        GeneratedFile {
            path: "src/main.rs".into(),
            contents: entry::render(ast),
        },
        GeneratedFile {
            path: "src/types.rs".into(),
            contents: types::render(ast)?,
        },
        GeneratedFile {
            path: "src/routes.rs".into(),
            contents: routes::render(ast),
        },
    ];
    if handlers::has_routes(ast) {
        files.push(GeneratedFile {
            path: "src/handlers/mod.rs".into(),
            contents: handlers::render_mod(ast),
        });
        files.extend(handlers::render_files(ast).into_iter().map(|(path, contents)| {
            GeneratedFile { path, contents }
        }));
    }
    Ok(files)
}
