use serde::Serialize;
use std::{fmt, ops::RangeInclusive};

/// A single machine-readable configuration or request validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Violation {
    pub field: String,
    pub code: &'static str,
    pub message: String,
}

/// Accumulates all validation failures instead of stopping at the first one.
#[derive(Debug, Default)]
pub struct Validation {
    violations: Vec<Violation>,
}

impl Validation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(
        &mut self,
        field: impl Into<String>,
        condition: bool,
        code: &'static str,
        message: impl Into<String>,
    ) -> &mut Self {
        if !condition {
            self.violations.push(Violation {
                field: field.into(),
                code,
                message: message.into(),
            });
        }
        self
    }

    pub fn required(&mut self, field: impl Into<String>, value: &str) -> &mut Self {
        self.check(
            field,
            !value.trim().is_empty(),
            "required",
            "must not be empty",
        )
    }

    pub fn length(
        &mut self,
        field: impl Into<String>,
        value: &str,
        range: RangeInclusive<usize>,
    ) -> &mut Self {
        let length = value.chars().count();
        let message = format!(
            "length must be between {} and {}",
            range.start(),
            range.end()
        );
        self.check(field, range.contains(&length), "length", message)
    }

    pub fn range<T>(
        &mut self,
        field: impl Into<String>,
        value: T,
        range: RangeInclusive<T>,
    ) -> &mut Self
    where
        T: PartialOrd + fmt::Display,
    {
        let message = format!("must be between {} and {}", range.start(), range.end());
        self.check(field, range.contains(&value), "range", message)
    }

    pub fn one_of<T>(&mut self, field: impl Into<String>, value: &T, allowed: &[T]) -> &mut Self
    where
        T: PartialEq + fmt::Display,
    {
        self.check(
            field,
            allowed.contains(value),
            "one_of",
            format!(
                "must be one of [{}]",
                allowed
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    }

    pub fn finish(self) -> Result<(), ValidationErrors> {
        if self.violations.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(self.violations))
        }
    }
}

/// Implemented by typed requests and configuration values that validate themselves.
pub trait Validate {
    fn validate(&self) -> Result<(), ValidationErrors>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrors(Vec<Violation>);

impl ValidationErrors {
    pub fn violations(&self) -> &[Violation] {
        &self.0
    }

    pub fn into_violations(self) -> Vec<Violation> {
        self.0
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, violation) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(
                formatter,
                "{}: {} ({})",
                violation.field, violation.message, violation.code
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_multiple_field_failures() {
        let mut validation = Validation::new();
        validation
            .required("name", " ")
            .range("age", 12, 18..=120)
            .one_of("mode", &"broken", &["dev", "prod"]);

        let errors = validation.finish().unwrap_err();
        assert_eq!(errors.violations().len(), 3);
        assert_eq!(errors.violations()[0].field, "name");
        assert_eq!(errors.violations()[1].code, "range");
        assert_eq!(errors.violations()[2].code, "one_of");
    }

    #[test]
    fn measures_string_length_in_characters() {
        let mut validation = Validation::new();
        validation.length("name", "你好", 2..=2);
        assert!(validation.finish().is_ok());
    }
}
