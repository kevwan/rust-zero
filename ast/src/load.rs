//! Load an entry `.api` file and the files it imports.

use crate::ast::{ApiFile, Service, TypeDef, TypeExpr};
use crate::parse::{parse, ParseError};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

const BUILTIN_TYPES: &[&str] = &[
    "String", "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8",
    "u16", "u32", "u64", "u128", "usize",
];

/// An entry file plus every imported type, already checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// Merged tree. `imports` is empty. `types` is entry then first-seen imports.
    /// `services` and `info` come only from the entry file.
    pub file: ApiFile,
    pub entry: PathBuf,
    /// Where each struct name was first declared.
    pub origins: BTreeMap<String, PathBuf>,
}

/// Failure while reading, parsing, or checking an import graph.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("read {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse {}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: ParseError,
    },
    #[error("{}", format_cycle(.stack))]
    Cycle { stack: Vec<PathBuf> },
    #[error("empty import path in {}", path.display())]
    EmptyImport { path: PathBuf },
    #[error("duplicate type '{name}': first in {}, also in {}", first.display(), second.display())]
    DuplicateType {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
    #[error("imported file {} must not declare a service", path.display())]
    ImportHasService { path: PathBuf },
    #[error("imported file {} must not declare info", path.display())]
    ImportHasInfo { path: PathBuf },
    #[error("imported file {} has syntax '{found}', expected '{expected}'", path.display())]
    SyntaxMismatch {
        path: PathBuf,
        expected: String,
        found: String,
    },
    #[error("duplicate route {method} {path}")]
    DuplicateRoute {
        method: String,
        path: String,
    },
    #[error("duplicate handler '{name}'")]
    DuplicateHandler { name: String },
    #[error("undefined type '{name}' used by {used_in}")]
    UndefinedType {
        name: String,
        used_in: String,
    },
}

fn format_cycle(stack: &[PathBuf]) -> String {
    let chain = stack
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(" -> ");
    format!("import cycle: {chain}")
}

/// Load `entry` and every imported `.api` file.
pub fn load(entry: impl AsRef<Path>) -> Result<Bundle, LoadError> {
    let entry = canonicalize(entry.as_ref())?;
    let mut loader = Loader {
        syntax: None,
        stack: Vec::new(),
        seen: HashSet::new(),
        files: HashMap::new(),
        type_order: Vec::new(),
        origins: BTreeMap::new(),
    };
    loader.walk(&entry, true)?;

    let entry_file = loader
        .files
        .get(&entry)
        .expect("entry file was loaded")
        .clone();
    let mut types = take_types(&entry_file);
    for path in &loader.type_order {
        if path != &entry {
            if let Some(file) = loader.files.get(path) {
                types.extend(file.types.iter().cloned());
            }
        }
    }

    let file = ApiFile {
        syntax: entry_file.syntax,
        imports: Vec::new(),
        info: entry_file.info,
        types,
        services: entry_file.services,
    };
    check_routes(&file.services)?;
    check_defined_types(&file)?;

    Ok(Bundle {
        file,
        entry,
        origins: loader.origins,
    })
}

struct Loader {
    syntax: Option<String>,
    stack: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
    files: HashMap<PathBuf, ApiFile>,
    type_order: Vec<PathBuf>,
    origins: BTreeMap<String, PathBuf>,
}

impl Loader {
    fn walk(&mut self, path: &Path, is_entry: bool) -> Result<(), LoadError> {
        if self.stack.iter().any(|seen| seen == path) {
            let mut stack = self.stack.clone();
            stack.push(path.to_path_buf());
            return Err(LoadError::Cycle { stack });
        }
        if !self.seen.insert(path.to_path_buf()) {
            return Ok(());
        }

        let source = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let file = parse(&source).map_err(|source| LoadError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        match &self.syntax {
            None => self.syntax = Some(file.syntax.version.clone()),
            Some(expected) if expected != &file.syntax.version => {
                return Err(LoadError::SyntaxMismatch {
                    path: path.to_path_buf(),
                    expected: expected.clone(),
                    found: file.syntax.version.clone(),
                });
            }
            Some(_) => {}
        }

        if !is_entry && !file.services.is_empty() {
            return Err(LoadError::ImportHasService {
                path: path.to_path_buf(),
            });
        }
        if !is_entry && file.info.is_some() {
            return Err(LoadError::ImportHasInfo {
                path: path.to_path_buf(),
            });
        }

        for def in &file.types {
            if let Some(first) = self.origins.get(&def.name) {
                return Err(LoadError::DuplicateType {
                    name: def.name.clone(),
                    first: first.clone(),
                    second: path.to_path_buf(),
                });
            }
            self.origins
                .insert(def.name.clone(), path.to_path_buf());
        }
        self.type_order.push(path.to_path_buf());

        self.stack.push(path.to_path_buf());
        let imports = file.imports.clone();
        let dir = path.parent().unwrap_or(Path::new("."));
        self.files.insert(path.to_path_buf(), file);
        for import in imports {
            if import.is_empty() {
                return Err(LoadError::EmptyImport {
                    path: path.to_path_buf(),
                });
            }
            let next = resolve_import(dir, &import)?;
            self.walk(&next, false)?;
        }
        self.stack.pop();
        Ok(())
    }
}

fn take_types(file: &ApiFile) -> Vec<TypeDef> {
    file.types.clone()
}

fn resolve_import(dir: &Path, import: &str) -> Result<PathBuf, LoadError> {
    let raw = Path::new(import);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        dir.join(raw)
    };
    canonicalize(&joined)
}

fn canonicalize(path: &Path) -> Result<PathBuf, LoadError> {
    path.canonicalize().map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn check_routes(services: &[Service]) -> Result<(), LoadError> {
    let mut routes = HashSet::new();
    let mut handlers = HashSet::new();
    for service in services {
        let prefix = server_item(service, "prefix")
            .map(normalize_prefix)
            .unwrap_or_default();
        for route in &service.routes {
            let path = join_path(&prefix, &route.path);
            let key = format!("{} {path}", route.method.as_str().to_ascii_uppercase());
            if !routes.insert(key.clone()) {
                return Err(LoadError::DuplicateRoute {
                    method: route.method.as_str().to_ascii_uppercase(),
                    path,
                });
            }
            let handler = handler_fn_name(route.handler.as_deref(), &route.path);
            if !handlers.insert(handler.clone()) {
                return Err(LoadError::DuplicateHandler { name: handler });
            }
        }
    }
    Ok(())
}

fn check_defined_types(file: &ApiFile) -> Result<(), LoadError> {
    let defined: HashSet<&str> = file.types.iter().map(|def| def.name.as_str()).collect();
    for def in &file.types {
        for field in &def.fields {
            check_type(&field.ty, &defined, &format!("struct {}", def.name))?;
        }
    }
    for service in &file.services {
        for route in &service.routes {
            let used_in = format!(
                "{} {}",
                route.method.as_str().to_ascii_uppercase(),
                route.path
            );
            if let Some(ty) = &route.request {
                check_type(ty, &defined, &used_in)?;
            }
            if let Some(ty) = &route.returns {
                check_type(ty, &defined, &used_in)?;
            }
        }
    }
    Ok(())
}

fn check_type(ty: &TypeExpr, defined: &HashSet<&str>, used_in: &str) -> Result<(), LoadError> {
    match ty {
        TypeExpr::Named(name) => {
            if BUILTIN_TYPES.contains(&name.as_str()) || defined.contains(name.as_str()) {
                Ok(())
            } else {
                Err(LoadError::UndefinedType {
                    name: name.clone(),
                    used_in: used_in.to_string(),
                })
            }
        }
        TypeExpr::List(inner) | TypeExpr::Optional(inner) => check_type(inner, defined, used_in),
        TypeExpr::Map { key, value } => {
            check_type(key, defined, used_in)?;
            check_type(value, defined, used_in)
        }
    }
}

fn server_item<'a>(service: &'a Service, key: &str) -> Option<&'a str> {
    service.server.as_ref().and_then(|server| {
        server
            .items
            .iter()
            .find(|item| item.key == key)
            .map(|item| item.value.as_str())
    })
}

fn normalize_prefix(prefix: &str) -> String {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        String::new()
    } else if prefix.starts_with('/') {
        prefix.trim_end_matches('/').to_string()
    } else {
        format!("/{}", prefix.trim_end_matches('/'))
    }
}

fn join_path(prefix: &str, path: &str) -> String {
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

fn handler_fn_name(handler: Option<&str>, path: &str) -> String {
    handler
        .map(str::to_string)
        .unwrap_or_else(|| handler_name_from_path(path))
}

fn handler_name_from_path(path: &str) -> String {
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
