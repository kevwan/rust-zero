use actix_web::{
    http::{header, Method},
    web::Bytes,
    HttpRequest, HttpResponse,
};
use std::{
    collections::HashMap,
    fmt, io,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// One in-memory asset served by [`StaticAssets`].
#[derive(Debug, Clone)]
pub struct EmbeddedAsset {
    body: Bytes,
    content_type: Arc<str>,
}

impl EmbeddedAsset {
    pub fn new(body: impl Into<Bytes>, content_type: impl Into<Arc<str>>) -> Self {
        Self {
            body: body.into(),
            content_type: content_type.into(),
        }
    }

    /// Creates an asset whose content type is inferred from the request path.
    pub fn inferred(body: impl Into<Bytes>) -> Self {
        Self::new(body, "")
    }
}

/// Errors raised while assembling a static fallback.
#[derive(Debug)]
pub enum StaticAssetsError {
    Directory { path: PathBuf, source: io::Error },
    InvalidPath(String),
    DuplicatePath(String),
    InvalidIndex,
}

impl fmt::Display for StaticAssetsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Directory { path, source } => {
                write!(
                    formatter,
                    "cannot use static directory {}: {source}",
                    path.display()
                )
            }
            Self::InvalidPath(path) => write!(formatter, "invalid embedded asset path: {path}"),
            Self::DuplicatePath(path) => write!(formatter, "duplicate embedded asset path: {path}"),
            Self::InvalidIndex => {
                formatter.write_str("static index file must be a single file name")
            }
        }
    }
}

impl std::error::Error for StaticAssetsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Directory { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Traversal-safe fallback for a static directory, embedded assets, or both.
///
/// Embedded assets take precedence over files in the directory. Explicit Actix routes still take
/// precedence because this handler is installed as the application's default service.
#[derive(Debug, Clone)]
pub struct StaticAssets {
    directory: Option<Arc<PathBuf>>,
    embedded: Arc<HashMap<String, EmbeddedAsset>>,
    index_file: Arc<str>,
}

impl Default for StaticAssets {
    fn default() -> Self {
        Self {
            directory: None,
            embedded: Arc::new(HashMap::new()),
            index_file: Arc::from("index.html"),
        }
    }
}

impl StaticAssets {
    /// Creates a fallback rooted at an existing directory. The root is canonicalized immediately.
    pub fn directory(root: impl AsRef<Path>) -> Result<Self, StaticAssetsError> {
        let supplied = root.as_ref();
        let canonical =
            std::fs::canonicalize(supplied).map_err(|source| StaticAssetsError::Directory {
                path: supplied.to_owned(),
                source,
            })?;
        if !canonical.is_dir() {
            return Err(StaticAssetsError::Directory {
                path: supplied.to_owned(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "path is not a directory"),
            });
        }
        Ok(Self {
            directory: Some(Arc::new(canonical)),
            ..Self::default()
        })
    }

    /// Creates an embedded-only fallback.
    pub fn embedded<I, P>(assets: I) -> Result<Self, StaticAssetsError>
    where
        I: IntoIterator<Item = (P, EmbeddedAsset)>,
        P: AsRef<str>,
    {
        let mut fallback = Self::default();
        for (path, asset) in assets {
            fallback.insert(path.as_ref(), asset)?;
        }
        Ok(fallback)
    }

    /// Adds or overlays an embedded asset.
    pub fn with_embedded(
        mut self,
        path: impl AsRef<str>,
        asset: EmbeddedAsset,
    ) -> Result<Self, StaticAssetsError> {
        self.insert(path.as_ref(), asset)?;
        Ok(self)
    }

    /// Changes the file served for a directory-style request such as `/` or `/docs/`.
    pub fn with_index_file(
        mut self,
        index_file: impl Into<String>,
    ) -> Result<Self, StaticAssetsError> {
        let index_file = index_file.into();
        if normalize_path(&index_file).as_deref() != Some(index_file.as_str())
            || index_file.contains('/')
        {
            return Err(StaticAssetsError::InvalidIndex);
        }
        self.index_file = Arc::from(index_file);
        Ok(self)
    }

    fn insert(&mut self, path: &str, asset: EmbeddedAsset) -> Result<(), StaticAssetsError> {
        let normalized =
            normalize_path(path).ok_or_else(|| StaticAssetsError::InvalidPath(path.to_owned()))?;
        if Arc::make_mut(&mut self.embedded)
            .insert(normalized.clone(), asset)
            .is_some()
        {
            return Err(StaticAssetsError::DuplicatePath(normalized));
        }
        Ok(())
    }

    pub(crate) async fn serve(&self, request: HttpRequest) -> HttpResponse {
        if request.method() != Method::GET && request.method() != Method::HEAD {
            return HttpResponse::NotFound().finish();
        }
        let Some(mut path) = normalize_path(request.path()) else {
            return HttpResponse::NotFound().finish();
        };
        if request.path().ends_with('/') || path.is_empty() {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(&self.index_file);
        }

        if let Some(asset) = self.embedded.get(&path) {
            let content_type = if asset.content_type.is_empty() {
                content_type(&path)
            } else {
                &asset.content_type
            };
            return response(request.method(), asset.body.clone(), content_type);
        }

        let Some(root) = &self.directory else {
            return HttpResponse::NotFound().finish();
        };
        let candidate = root.join(&path);
        let Ok(canonical) = tokio::fs::canonicalize(candidate).await else {
            return HttpResponse::NotFound().finish();
        };
        if !canonical.starts_with(root.as_ref()) {
            return HttpResponse::NotFound().finish();
        }
        let Ok(metadata) = tokio::fs::metadata(&canonical).await else {
            return HttpResponse::NotFound().finish();
        };
        if !metadata.is_file() {
            return HttpResponse::NotFound().finish();
        }
        match tokio::fs::read(canonical).await {
            Ok(body) => response(request.method(), Bytes::from(body), content_type(&path)),
            Err(_) => HttpResponse::NotFound().finish(),
        }
    }
}

fn normalize_path(path: &str) -> Option<String> {
    if path.contains('\0') || path.contains('\\') {
        return None;
    }
    let path = path.trim_start_matches('/');
    let mut normalized = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(value) => normalized.push(value.to_str()?.to_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized.join("/"))
}

fn response(method: &Method, body: Bytes, content_type: &str) -> HttpResponse {
    let length = body.len();
    let mut builder = HttpResponse::Ok();
    builder.insert_header((header::CONTENT_TYPE, content_type));
    builder.insert_header((header::CONTENT_LENGTH, length));
    if method == Method::HEAD {
        builder.finish()
    } else {
        builder.body(body)
    }
}

fn content_type(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("wasm") => "application/wasm",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{body::to_bytes, http::StatusCode, test};
    use std::fs;

    #[actix_web::test]
    async fn serves_embedded_index_assets_and_head_requests() {
        let assets = StaticAssets::embedded([
            ("index.html", EmbeddedAsset::inferred("home")),
            ("/app.js", EmbeddedAsset::inferred("code")),
        ])
        .unwrap();

        let index = assets
            .serve(test::TestRequest::get().uri("/").to_http_request())
            .await;
        assert_eq!(index.status(), StatusCode::OK);
        assert_eq!(
            index.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(to_bytes(index.into_body()).await.unwrap(), "home");

        let head = assets
            .serve(
                test::TestRequest::default()
                    .method(Method::HEAD)
                    .uri("/app.js")
                    .to_http_request(),
            )
            .await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers().get(header::CONTENT_LENGTH).unwrap(), "4");
        assert!(to_bytes(head.into_body()).await.unwrap().is_empty());
    }

    #[actix_web::test]
    async fn serves_directory_files_and_rejects_traversal() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("index.html"), "directory home").unwrap();
        fs::write(directory.path().join("data.json"), "{}").unwrap();
        let assets = StaticAssets::directory(directory.path()).unwrap();

        let file = assets
            .serve(test::TestRequest::get().uri("/data.json").to_http_request())
            .await;
        assert_eq!(file.status(), StatusCode::OK);
        assert_eq!(
            file.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );

        for uri in ["/../Cargo.toml", "/missing", "/data.json/child"] {
            let response = assets
                .serve(test::TestRequest::get().uri(uri).to_http_request())
                .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[cfg(unix)]
    #[actix_web::test]
    async fn rejects_symlinks_that_escape_the_static_root() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "secret").unwrap();
        symlink(outside.path(), directory.path().join("escape.txt")).unwrap();
        let assets = StaticAssets::directory(directory.path()).unwrap();

        let response = assets
            .serve(
                test::TestRequest::get()
                    .uri("/escape.txt")
                    .to_http_request(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn validates_embedded_paths_and_duplicates() {
        assert!(StaticAssets::embedded([("../secret", EmbeddedAsset::inferred("x"))]).is_err());
        assert!(StaticAssets::embedded([
            ("same", EmbeddedAsset::inferred("a")),
            ("/same", EmbeddedAsset::inferred("b")),
        ])
        .is_err());
    }
}
