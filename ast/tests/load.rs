//! File-driven load suite. Cases live in testdata; this file only runs them.
//!
//! Long files go in fixtures/. Write `file: name.api` as the Input.

use ast::load;
use std::fs;
use std::path::PathBuf;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_path(name: &str) -> PathBuf {
    crate_dir().join("tests/fixtures").join(name)
}

fn testdata_path(name: &str) -> PathBuf {
    crate_dir().join("tests/testdata").join(name)
}

fn resolve_input(raw: &str) -> PathBuf {
    let raw = raw.trim();
    let Some(name) = raw.strip_prefix("file:") else {
        panic!("load cases must use file: fixtures, got {raw}");
    };
    fixture_path(name.trim())
}

fn split_cases(raw: &str, end_marker: &str) -> Vec<(String, String)> {
    let mut cases = Vec::new();
    for chunk in raw.split("---------- Input ----------") {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let Some((input, rest)) = chunk.split_once(end_marker) else {
            panic!("case missing {end_marker}: {chunk}");
        };
        cases.push((input.trim().to_string(), rest.trim().to_string()));
    }
    cases
}

#[test]
fn test_load_ok() {
    let raw = fs::read_to_string(testdata_path("load_ok.txt")).unwrap();
    let cases = split_cases(&raw, "---------- Check -----------");
    assert!(!cases.is_empty(), "load_ok.txt has no cases");
    for (input, expected) in cases {
        let path = resolve_input(&input);
        let bundle = load(&path).unwrap_or_else(|err| panic!("load ok failed: {err}\n{input}"));
        for line in expected.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(names) = line.strip_prefix("types: ") {
                let got: Vec<_> = bundle.file.types.iter().map(|def| def.name.as_str()).collect();
                let want: Vec<_> = names.split(',').map(str::trim).collect();
                assert_eq!(got, want, "types mismatch for {input}");
            } else if let Some(names) = line.strip_prefix("services: ") {
                let got: Vec<_> = bundle
                    .file
                    .services
                    .iter()
                    .map(|service| service.name.as_str())
                    .collect();
                let want: Vec<_> = names.split(',').map(str::trim).collect();
                assert_eq!(got, want, "services mismatch for {input}");
            } else if let Some(count) = line.strip_prefix("imports: ") {
                let want: usize = count.parse().unwrap();
                assert_eq!(
                    bundle.file.imports.len(),
                    want,
                    "imports mismatch for {input}"
                );
            } else if let Some(rest) = line.strip_prefix("origin ") {
                let (name, file) = rest
                    .split_once(':')
                    .unwrap_or_else(|| panic!("bad origin line: {line}"));
                let name = name.trim();
                let file = file.trim();
                let origin = bundle
                    .origins
                    .get(name)
                    .unwrap_or_else(|| panic!("missing origin {name} for {input}"));
                assert!(
                    origin.ends_with(file),
                    "origin {name} is {}, expected to end with {file}",
                    origin.display()
                );
            } else {
                panic!("unknown check line: {line}");
            }
        }
    }
}

#[test]
fn test_load_err() {
    let raw = include_str!("testdata/load_err.txt");
    let cases = split_cases(raw, "---------- Error -----------");
    assert!(!cases.is_empty(), "load_err.txt has no cases");
    for (input, needle) in cases {
        let path = resolve_input(&input);
        let err = load(&path).expect_err(&format!("expected error for:\n{input}"));
        let msg = err.to_string();
        assert!(
            msg.contains(&needle),
            "error {msg:?} does not contain {needle:?}\n{input}"
        );
    }
}
