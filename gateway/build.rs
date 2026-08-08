fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .file_descriptor_set_path(
            std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("gateway.bin"),
        )
        .compile_protos(&["proto/transcoding.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/transcoding.proto");
    Ok(())
}
