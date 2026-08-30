use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[command(name = "rust-zero", version, about = "Scaffold rust-zero services")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a runnable REST service project.
    New(NewArgs),
    /// Create Tonic protobuf build scaffolding around an existing .proto file.
    Proto(ProtoArgs),
    /// Generate Actix route stubs from an OpenAPI JSON or YAML document.
    Openapi(OpenapiArgs),
    /// Generate a production multi-stage Dockerfile.
    Docker(DockerArgs),
    /// Generate Kubernetes Deployment and Service resources.
    Kubernetes(KubernetesArgs),
}

#[derive(Debug, Args)]
struct NewArgs {
    name: String,
    #[arg(long)]
    dir: Option<PathBuf>,
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct ProtoArgs {
    proto: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct OpenapiArgs {
    spec: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct DockerArgs {
    #[arg(long)]
    binary: String,
    #[arg(long, default_value = "Dockerfile")]
    out: PathBuf,
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct KubernetesArgs {
    #[arg(long)]
    name: String,
    #[arg(long)]
    image: String,
    #[arg(long, default_value_t = 8080)]
    port: u16,
    #[arg(long, default_value = "kubernetes.yaml")]
    out: PathBuf,
    #[arg(long)]
    force: bool,
}

#[derive(Debug)]
struct CliError(String);

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CliError {}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error}");
        std::process::exit(2);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.command {
        Command::New(args) => create_project(args)?,
        Command::Proto(args) => create_proto_scaffold(args)?,
        Command::Openapi(args) => generate_openapi(args)?,
        Command::Docker(args) => generate_dockerfile(args)?,
        Command::Kubernetes(args) => generate_kubernetes(args)?,
    }
    Ok(())
}

fn create_project(args: NewArgs) -> Result<(), Box<dyn Error>> {
    validate_name(&args.name)?;
    let root = args.dir.unwrap_or_else(|| PathBuf::from(&args.name));
    let package = args.name.replace('_', "-");
    let source = root.join("src/main.rs");
    write_generated(
        &root.join("Cargo.toml"),
        &format!(
            r#"[package]
name = "{package}"
version = "0.1.0"
edition = "2021"

[dependencies]
actix-web = "4"
rust-zero-rest = "{VERSION}"
"#
        ),
        args.force,
    )?;
    write_generated(
        &source,
        r#"use actix_web::{web, HttpResponse};
use rest::{RestServer, RestServerConfig};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let config = RestServerConfig::default();
    RestServer::new(config)
        .map_err(std::io::Error::other)?
        .run(|routes| {
            routes.route("/healthz", web::get().to(|| async { HttpResponse::Ok().finish() }));
        })?
        .await
}
"#,
        args.force,
    )?;
    write_generated(&root.join(".gitignore"), "/target\n.env\n", args.force)?;
    println!("created {}", root.display());
    Ok(())
}

fn create_proto_scaffold(args: ProtoArgs) -> Result<(), Box<dyn Error>> {
    let file_name = args
        .proto
        .file_name()
        .ok_or_else(|| CliError("proto path must name a file".to_owned()))?;
    if args.proto.extension().and_then(|value| value.to_str()) != Some("proto") {
        return Err(CliError("proto input must have a .proto extension".to_owned()).into());
    }
    let contents = fs::read_to_string(&args.proto)?;
    let proto_dir = args.out.join("proto");
    write_generated(&proto_dir.join(file_name), &contents, args.force)?;
    write_generated(
        &args.out.join("build.rs"),
        &format!(
            r#"fn main() -> Result<(), Box<dyn std::error::Error>> {{
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);
    tonic_prost_build::configure().compile_protos("proto/{}")?;
    println!("cargo:rerun-if-changed=proto/{}");
    Ok(())
}}
"#,
            file_name.to_string_lossy(),
            file_name.to_string_lossy()
        ),
        args.force,
    )?;
    write_generated(
        &args.out.join("PROTO_DEPENDENCIES.toml"),
        &format!(
            r#"# Merge these sections into the service Cargo.toml.
[dependencies]
prost = "0.14"
rust-zero-rpc = "{VERSION}"
tonic = {{ version = "0.14", features = ["transport"] }}
tonic-prost = "0.14"

[build-dependencies]
protoc-bin-vendored = "3"
tonic-prost-build = "0.14"
"#,
        ),
        args.force,
    )?;
    println!("generated protobuf scaffold in {}", args.out.display());
    Ok(())
}

fn generate_openapi(args: OpenapiArgs) -> Result<(), Box<dyn Error>> {
    let source = fs::read_to_string(&args.spec)?;
    let document: Value = match args.spec.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(&source)?,
        Some("yaml" | "yml") => serde_yaml::from_str(&source)?,
        _ => return Err(CliError("OpenAPI input must be JSON, YAML, or YML".to_owned()).into()),
    };
    if !document
        .get("openapi")
        .and_then(Value::as_str)
        .is_some_and(|version| version.starts_with('3'))
    {
        return Err(CliError("only OpenAPI 3.x documents are supported".to_owned()).into());
    }
    let paths = document
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| CliError("OpenAPI document must contain a paths object".to_owned()))?;
    let mut operations = Vec::new();
    for (path, item) in paths {
        let Some(methods) = item.as_object() else {
            continue;
        };
        for method in ["get", "post", "put", "patch", "delete", "head", "options"] {
            let Some(operation) = methods.get(method) else {
                continue;
            };
            let name = operation
                .get("operationId")
                .and_then(Value::as_str)
                .map(rust_identifier)
                .unwrap_or_else(|| rust_identifier(&format!("{method}_{path}")));
            operations.push((path.clone(), method.to_owned(), name));
        }
    }
    operations.sort();
    let mut generated = String::from(
        "// Generated by rust-zero openapi. Business logic is intentionally left as stubs.\n\
use actix_web::{web, HttpResponse};\n\n\
pub fn configure(routes: &mut web::ServiceConfig) {\n",
    );
    for (path, method, name) in &operations {
        generated.push_str(&format!(
            "    routes.route({path:?}, web::{method}().to({name}));\n"
        ));
    }
    generated.push_str("}\n\n");
    for (_, _, name) in &operations {
        generated.push_str(&format!(
            "async fn {name}() -> HttpResponse {{\n    HttpResponse::NotImplemented().finish()\n}}\n\n"
        ));
    }
    write_generated(&args.out, &generated, args.force)?;
    println!(
        "generated {} route stubs in {}",
        operations.len(),
        args.out.display()
    );
    Ok(())
}

fn generate_dockerfile(args: DockerArgs) -> Result<(), Box<dyn Error>> {
    validate_name(&args.binary)?;
    let content = format!(
        r#"FROM rust:1.89-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --locked --release --bin {0}

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/{0} /usr/local/bin/{0}
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/{0}"]
"#,
        args.binary
    );
    write_generated(&args.out, &content, args.force)?;
    println!("generated {}", args.out.display());
    Ok(())
}

fn generate_kubernetes(args: KubernetesArgs) -> Result<(), Box<dyn Error>> {
    validate_dns_name(&args.name)?;
    if args.image.trim().is_empty() || args.image.contains(char::is_whitespace) {
        return Err(CliError("image must be a non-empty container reference".to_owned()).into());
    }
    let content = format!(
        r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: {0}
spec:
  replicas: 2
  selector:
    matchLabels:
      app: {0}
  template:
    metadata:
      labels:
        app: {0}
    spec:
      containers:
        - name: {0}
          image: {1}
          ports:
            - name: http
              containerPort: {2}
          readinessProbe:
            httpGet:
              path: /healthz
              port: http
          livenessProbe:
            httpGet:
              path: /healthz
              port: http
          resources:
            requests:
              cpu: 100m
              memory: 128Mi
            limits:
              cpu: "1"
              memory: 512Mi
---
apiVersion: v1
kind: Service
metadata:
  name: {0}
spec:
  selector:
    app: {0}
  ports:
    - name: http
      port: {2}
      targetPort: http
"#,
        args.name, args.image, args.port
    );
    write_generated(&args.out, &content, args.force)?;
    println!("generated {}", args.out.display());
    Ok(())
}

fn write_generated(path: &Path, contents: &str, force: bool) -> Result<(), Box<dyn Error>> {
    if path.exists() && !force {
        return Err(CliError(format!(
            "{} already exists; pass --force to replace it",
            path.display()
        ))
        .into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    Ok(())
}

fn validate_name(name: &str) -> Result<(), CliError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(CliError(
            "name must contain only ASCII letters, digits, '-' or '_'".to_owned(),
        ));
    }
    Ok(())
}

fn validate_dns_name(name: &str) -> Result<(), CliError> {
    if name.is_empty()
        || name.len() > 63
        || name.starts_with('-')
        || name.ends_with('-')
        || !name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        return Err(CliError(
            "Kubernetes name must be a lowercase DNS label of at most 63 characters".to_owned(),
        ));
    }
    Ok(())
}

fn rust_identifier(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if output.is_empty() && character.is_ascii_digit() {
                output.push('_');
            }
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else if !output.is_empty() && !separator {
            output.push('_');
            separator = true;
        }
    }
    while output.ends_with('_') {
        output.pop();
    }
    if output.is_empty() {
        "operation".to_owned()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_creation_is_non_overwriting() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("users");
        create_project(NewArgs {
            name: "users".to_owned(),
            dir: Some(root.clone()),
            force: false,
        })
        .unwrap();
        assert!(root.join("src/main.rs").exists());
        assert!(create_project(NewArgs {
            name: "users".to_owned(),
            dir: Some(root),
            force: false,
        })
        .is_err());
    }

    #[test]
    fn protobuf_scaffold_uses_the_workspace_rpc_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let proto = temporary.path().join("echo.proto");
        fs::write(
            &proto,
            "syntax = \"proto3\";\npackage echo;\nmessage Request {}\n",
        )
        .unwrap();
        let output = temporary.path().join("generated");

        create_proto_scaffold(ProtoArgs {
            proto,
            out: output.clone(),
            force: false,
        })
        .unwrap();

        let build_script = fs::read_to_string(output.join("build.rs")).unwrap();
        let dependencies = fs::read_to_string(output.join("PROTO_DEPENDENCIES.toml")).unwrap();
        assert!(build_script.contains("tonic_prost_build::configure()"));
        assert!(dependencies.contains("prost = \"0.14\""));
        assert!(dependencies.contains("tonic-prost = \"0.14\""));
        assert!(dependencies.contains("tonic-prost-build = \"0.14\""));
        assert!(!dependencies.contains("tonic-build = \"0.12\""));
    }

    #[test]
    fn openapi_generation_is_deterministic() {
        let temporary = tempfile::tempdir().unwrap();
        let spec = temporary.path().join("api.yaml");
        fs::write(
            &spec,
            "openapi: 3.1.0\npaths:\n  /users/{id}:\n    get:\n      operationId: getUser\n  /users:\n    post: {}\n",
        )
        .unwrap();
        let output = temporary.path().join("routes.rs");
        generate_openapi(OpenapiArgs {
            spec,
            out: output.clone(),
            force: false,
        })
        .unwrap();
        let generated = fs::read_to_string(output).unwrap();
        assert!(generated.contains("web::get().to(getuser)"));
        assert!(generated.contains("web::post().to(post_users)"));
    }

    #[test]
    fn deployment_generators_validate_names_and_do_not_overwrite() {
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("kubernetes.yaml");
        generate_kubernetes(KubernetesArgs {
            name: "users-api".to_owned(),
            image: "example/users:1".to_owned(),
            port: 8080,
            out: output.clone(),
            force: false,
        })
        .unwrap();
        assert!(fs::read_to_string(&output)
            .unwrap()
            .contains("readinessProbe"));
        assert!(generate_kubernetes(KubernetesArgs {
            name: "INVALID".to_owned(),
            image: "example/users:1".to_owned(),
            port: 8080,
            out: output,
            force: true,
        })
        .is_err());
    }
}
