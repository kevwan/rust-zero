//! File-driven codegen suite. Cases live in testdata as `.api` inputs.
//!
//! Default: parse and assert the service-crate layout in memory.
//! Write trees for inspection:
//!   UPDATE_CODEGEN=1 cargo test -p rust-zero-codegen --test codegen test_generate_ok

use ast::parse;
use codegen::generate;
use std::fs;
use std::path::PathBuf;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn testdata_dir() -> PathBuf {
    crate_dir().join("tests/testdata")
}

#[test]
fn test_generate_ok() {
    let dir = testdata_dir();
    let mut cases: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("read testdata: {err}"))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("api") {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "testdata has no .api cases");
    let update = std::env::var_os("UPDATE_CODEGEN").is_some();

    for api_path in cases {
        let stem = api_path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("utf-8 case name");
        let src =
            fs::read_to_string(&api_path).unwrap_or_else(|err| panic!("read {stem}.api: {err}"));
        let file = parse(&src).unwrap_or_else(|err| panic!("parse {stem}.api: {err}"));
        let generated = generate(&file);
        if update {
            let out_dir = dir.join(stem);
            if out_dir.exists() {
                fs::remove_dir_all(&out_dir).unwrap();
            }
            for item in &generated {
                let path = out_dir.join(&item.path);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(path, &item.contents).unwrap();
            }
        }
        assert!(
            generated.iter().any(|item| item.path == "Cargo.toml"),
            "{stem}: missing Cargo.toml"
        );
        assert!(
            generated.iter().any(|item| item.path == "src/main.rs"),
            "{stem}: missing src/main.rs"
        );
        assert!(
            generated.iter().any(|item| item.path == "src/types.rs"),
            "{stem}: missing src/types.rs"
        );
        assert!(
            generated.iter().any(|item| item.path == "src/routes.rs"),
            "{stem}: missing src/routes.rs"
        );
        assert!(
            generated
                .iter()
                .any(|item| item.path == "src/handlers/mod.rs"),
            "{stem}: missing src/handlers/mod.rs"
        );
        for item in &generated {
            assert!(
                item.path == "Cargo.toml" || item.path.starts_with("src/"),
                "{stem}: unexpected path {}",
                item.path
            );
            assert!(
                !item.contents.trim().is_empty(),
                "{stem}: empty file {}",
                item.path
            );
        }
        let main = file_named(&generated, "src/main.rs");
        assert!(
            main.contains("RestServer"),
            "{stem}: main does not use RestServer"
        );
        assert!(
            main.contains("route_groups: routes::route_groups()"),
            "{stem}: main does not install route groups"
        );
        let routes = file_named(&generated, "src/routes.rs");
        assert!(
            routes.contains("pub fn route_groups()"),
            "{stem}: routes.rs missing route_groups"
        );
        let manifest = file_named(&generated, "Cargo.toml");
        assert!(
            manifest.contains("rust-zero-rest"),
            "{stem}: Cargo.toml missing rust-zero-rest"
        );
    }
}

fn file_named<'a>(files: &'a [codegen::GeneratedFile], path: &str) -> &'a str {
    files
        .iter()
        .find(|item| item.path == path)
        .map(|item| item.contents.as_str())
        .unwrap_or_else(|| panic!("missing generated file {path}"))
}
