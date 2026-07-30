use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

/// The supported serialized configuration formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Json,
    Toml,
    Yaml,
}

impl ConfigFormat {
    fn from_path(path: &Path) -> Result<Self, ConfigError> {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("json") => Ok(Self::Json),
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

/// Parses a typed configuration value from JSON, TOML, or YAML.
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
                "unsupported configuration format for {}; use .json, .toml, .yaml, or .yml",
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
}
