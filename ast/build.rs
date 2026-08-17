use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let grammar = manifest_dir.join("grammar.lalrpop");

    println!("cargo:rerun-if-changed={}", grammar.display());
    lalrpop::Configuration::new()
        .use_cargo_dir_conventions()
        .set_in_dir(&manifest_dir)
        .set_out_dir(&out_dir)
        .process_file(&grammar)
        .unwrap();
}
