//! File-driven parse suite. Cases live in testdata; this file only runs them.
//!
//! Add a case:
//!   1. Short snippet: paste under `---------- Input ----------` in parse_ok.txt / parse_err.txt
//!   2. Long file: put it in fixtures/, write `file: name.api` as the Input
//! Then run `UPDATE_AST=1 cargo test -p rust-zero-ast --test parser` to refresh AST goldens.

use ast::parse;
use std::fs;
use std::path::PathBuf;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_path(name: &str) -> PathBuf {
    crate_dir().join("tests/fixtures").join(name)
}

fn resolve_input(raw: &str) -> (String, String) {
    let raw = raw.trim_end_matches('\n');
    if let Some(name) = raw.strip_prefix("file:") {
        let name = name.trim();
        let src = fs::read_to_string(fixture_path(name))
            .unwrap_or_else(|err| panic!("read fixture {name}: {err}"));
        (format!("file: {name}"), src)
    } else {
        (raw.to_string(), raw.to_string())
    }
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

fn testdata_path(name: &str) -> PathBuf {
    crate_dir().join("tests/testdata").join(name)
}

#[test]
fn test_parse_ok() {
    let path = testdata_path("parse_ok.txt");
    let raw = fs::read_to_string(&path).unwrap();
    let cases = split_cases(&raw, "---------- AST ------------");
    assert!(!cases.is_empty(), "parse_ok.txt has no cases");

    let update = std::env::var_os("UPDATE_AST").is_some();
    let mut rebuilt = String::new();
    for (input, expected_ast) in &cases {
        let (label, src) = resolve_input(input);
        let file = parse(&src).unwrap_or_else(|err| panic!("parse ok failed: {err}\n{label}"));
        let got = format!("{file:#?}");
        if update {
            rebuilt.push_str("---------- Input ----------\n");
            rebuilt.push_str(&label);
            rebuilt.push('\n');
            rebuilt.push_str("---------- AST ------------\n");
            rebuilt.push_str(&got);
            rebuilt.push_str("\n\n\n");
        } else {
            assert_eq!(got, *expected_ast, "AST mismatch for:\n{label}");
        }
    }
    if update {
        fs::write(&path, rebuilt).unwrap();
    }
}

#[test]
fn test_parse_err() {
    let raw = include_str!("testdata/parse_err.txt");
    let cases = split_cases(raw, "---------- Error -----------");
    assert!(!cases.is_empty(), "parse_err.txt has no cases");
    for (input, needle) in cases {
        let (_label, src) = resolve_input(&input);
        let err = parse(&src).expect_err(&format!("expected error for:\n{input}"));
        let msg = err.to_string();
        assert!(
            msg.contains(&needle),
            "error {msg:?} does not contain {needle:?}\n{input}"
        );
    }
}
