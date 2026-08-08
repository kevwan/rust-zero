use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

/// The supported serialized configuration formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Json,
    Json5,
    Toml,
    Yaml,
}

impl ConfigFormat {
    fn from_path(path: &Path) -> Result<Self, ConfigError> {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("json") => Ok(Self::Json),
            Some("json5") => Ok(Self::Json5),
            Some("toml") => Ok(Self::Toml),
            Some("yaml" | "yml") => Ok(Self::Yaml),
            _ => Err(ConfigError::UnsupportedFormat(path.to_path_buf())),
        }
    }
}

/// Loads a typed configuration value based on the file extension.
pub fn load_config<T: DeserializeOwned>(path: impl AsRef<Path>) -> Result<T, ConfigError> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_config(&contents, ConfigFormat::from_path(path)?)
}

/// Parses a typed configuration value from JSON, JSON5, TOML, or YAML.
///
/// `${VAR}` and `$VAR` references are expanded from the process environment before parsing.
pub fn parse_config<T: DeserializeOwned>(
    contents: &str,
    format: ConfigFormat,
) -> Result<T, ConfigError> {
    let contents = expand_environment(contents)?;
    match format {
        ConfigFormat::Json => {
            serde_json::from_str(&contents).map_err(|error| ConfigError::Parse(error.to_string()))
        }
        ConfigFormat::Json5 => {
            json5::from_str(&contents).map_err(|error| ConfigError::Parse(error.to_string()))
        }
        ConfigFormat::Toml => {
            toml::from_str(&contents).map_err(|error| ConfigError::Parse(error.to_string()))
        }
        ConfigFormat::Yaml => {
            serde_yaml::from_str(&contents).map_err(|error| ConfigError::Parse(error.to_string()))
        }
    }
}

fn expand_environment(input: &str) -> Result<String, ConfigError> {
    let mut expanded = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();

    while let Some(character) = characters.next() {
        if character != '$' {
            expanded.push(character);
            continue;
        }

        let name = if characters.peek() == Some(&'{') {
            characters.next();
            let mut name = String::new();
            loop {
                match characters.next() {
                    Some('}') => break name,
                    Some(character) => name.push(character),
                    None => return Err(ConfigError::InvalidEnvironmentReference),
                }
            }
        } else {
            let mut name = String::new();
            while matches!(characters.peek(), Some(character) if character.is_ascii_alphanumeric() || *character == '_')
            {
                name.push(characters.next().expect("peeked character must exist"));
            }
            name
        };

        if name.is_empty() {
            expanded.push('$');
            continue;
        }

        let value = env::var(&name).map_err(|_| ConfigError::MissingEnvironmentVariable(name))?;
        expanded.push_str(&value);
    }

    Ok(expanded)
}

/// Common service metadata used by REST, RPC, gateway, and background services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    #[serde(default = "default_host")]
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub mode: ServiceMode,
}

impl ServiceConfig {
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn default_host() -> String {
    "0.0.0.0".to_owned()
}

/// Service operating mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceMode {
    Development,
    #[default]
    Production,
    Test,
}

/// Errors produced while loading or parsing configuration.
#[derive(Debug)]
pub enum ConfigError {
    Io { path: PathBuf, source: io::Error },
    UnsupportedFormat(PathBuf),
    InvalidEnvironmentReference,
    MissingEnvironmentVariable(String),
    Parse(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "failed to read configuration {}: {source}",
                    path.display()
                )
            }
            Self::UnsupportedFormat(path) => write!(
                formatter,
                "unsupported configuration format for {}; use .json, .json5, .toml, .yaml, or .yml",
                path.display()
            ),
            Self::InvalidEnvironmentReference => {
                formatter.write_str("unterminated ${VAR} configuration reference")
            }
            Self::MissingEnvironmentVariable(name) => {
                write!(
                    formatter,
                    "configuration references missing environment variable {name}"
                )
            }
            Self::Parse(error) => write!(formatter, "failed to parse configuration: {error}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::error::Error as _;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Credentials {
        username: String,
        password: String,
    }

    #[test]
    fn parses_toml_with_environment_expansion() {
        unsafe {
            env::set_var("RUST_ZERO_CONFIG_PASSWORD", "correct-horse-battery-staple");
        }

        let credentials: Credentials = parse_config(
            "username = \"service\"\npassword = \"${RUST_ZERO_CONFIG_PASSWORD}\"",
            ConfigFormat::Toml,
        )
        .unwrap();

        assert_eq!(
            credentials,
            Credentials {
                username: "service".to_owned(),
                password: "correct-horse-battery-staple".to_owned(),
            }
        );

        unsafe {
            env::remove_var("RUST_ZERO_CONFIG_PASSWORD");
        }
    }

    #[test]
    fn reports_missing_environment_values() {
        let error = parse_config::<Credentials>(
            "username: service\npassword: ${RUST_ZERO_MISSING_VALUE}",
            ConfigFormat::Yaml,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::MissingEnvironmentVariable(name) if name == "RUST_ZERO_MISSING_VALUE"
        ));
    }

    #[test]
    fn service_config_defaults_to_production_on_all_interfaces() {
        let config: ServiceConfig =
            parse_config("name = \"users\"\nport = 8080", ConfigFormat::Toml).unwrap();

        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.mode, ServiceMode::Production);
        assert_eq!(config.address(), "0.0.0.0:8080");
    }

    #[test]
    fn parses_json_yaml_and_unbraced_environment_references() {
        unsafe {
            env::set_var("RUST_ZERO_CONFIG_USER", "worker");
        }

        let json: Credentials = parse_config(
            r#"{"username":"$RUST_ZERO_CONFIG_USER","password":"secret"}"#,
            ConfigFormat::Json,
        )
        .unwrap();
        let yaml: Credentials =
            parse_config("username: worker\npassword: secret", ConfigFormat::Yaml).unwrap();

        assert_eq!(json, yaml);
        assert_eq!(expand_environment("$ ${}").unwrap(), "$ $");

        unsafe {
            env::remove_var("RUST_ZERO_CONFIG_USER");
        }
    }

    #[test]
    fn parses_json5_comments_trailing_commas_and_unquoted_keys() {
        let credentials: Credentials = parse_config(
            r#"{
                // JSON5 configuration can remain friendly to humans.
                username: 'service',
                password: 'secret',
            }"#,
            ConfigFormat::Json5,
        )
        .unwrap();

        assert_eq!(
            credentials,
            Credentials {
                username: "service".to_owned(),
                password: "secret".to_owned(),
            }
        );
    }

    #[test]
    fn loads_supported_file_extensions() {
        let directory = env::temp_dir();
        let process = std::process::id();
        let fixtures = [
            ("json", r#"{"username":"service","password":"secret"}"#),
            ("json5", "{username: 'service', password: 'secret',}"),
            ("toml", "username = \"service\"\npassword = \"secret\""),
            ("yaml", "username: service\npassword: secret"),
            ("yml", "username: service\npassword: secret"),
        ];

        for (extension, contents) in fixtures {
            let path = directory.join(format!("rust-zero-config-{process}.{extension}"));
            fs::write(&path, contents).unwrap();
            let credentials: Credentials = load_config(&path).unwrap();
            assert_eq!(credentials.username, "service");
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn reports_io_format_reference_and_parse_errors() {
        let missing = env::temp_dir().join(format!(
            "rust-zero-missing-config-{}.json",
            std::process::id()
        ));
        let io_error = load_config::<Credentials>(&missing).unwrap_err();
        assert!(matches!(io_error, ConfigError::Io { .. }));
        assert!(io_error.source().is_some());
        assert!(io_error
            .to_string()
            .contains("failed to read configuration"));

        let unsupported =
            env::temp_dir().join(format!("rust-zero-config-{}.txt", std::process::id()));
        fs::write(&unsupported, "{}").unwrap();
        let format_error = load_config::<Credentials>(&unsupported).unwrap_err();
        fs::remove_file(unsupported).unwrap();
        assert!(matches!(format_error, ConfigError::UnsupportedFormat(_)));
        assert!(format_error
            .to_string()
            .contains("use .json, .json5, .toml, .yaml"));
        assert!(format_error.source().is_none());

        let reference_error =
            parse_config::<Credentials>("${UNCLOSED", ConfigFormat::Json).unwrap_err();
        assert!(matches!(
            reference_error,
            ConfigError::InvalidEnvironmentReference
        ));
        assert_eq!(
            reference_error.to_string(),
            "unterminated ${VAR} configuration reference"
        );

        let parse_error = parse_config::<Credentials>("not json", ConfigFormat::Json).unwrap_err();
        assert!(matches!(parse_error, ConfigError::Parse(_)));
        assert!(parse_error
            .to_string()
            .starts_with("failed to parse configuration:"));
    }
}
