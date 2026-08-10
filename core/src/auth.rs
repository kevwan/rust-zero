//! Transport-neutral authentication results and time-window-bounded request signatures.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

/// Transport-neutral HS256 validation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtValidationError {
    Malformed,
    UnsupportedAlgorithm,
    InvalidSignature,
    Expired,
    NotYetValid,
    InvalidClaims,
}

impl From<JwtValidationError> for AuthFailure {
    fn from(error: JwtValidationError) -> Self {
        match error {
            JwtValidationError::Malformed
            | JwtValidationError::UnsupportedAlgorithm
            | JwtValidationError::InvalidClaims => Self::MalformedCredentials,
            JwtValidationError::InvalidSignature => Self::InvalidCredentials,
            JwtValidationError::Expired => Self::ExpiredCredentials,
            JwtValidationError::NotYetValid => Self::NotYetValid,
        }
    }
}

pub fn decode_jwt_hs256<T>(
    token: &str,
    secrets: &[Arc<[u8]>],
    leeway_seconds: u64,
    now_unix_seconds: u64,
) -> Result<T, JwtValidationError>
where
    T: DeserializeOwned,
{
    let mut segments = token.split('.');
    let header = segments.next().ok_or(JwtValidationError::Malformed)?;
    let claims = segments.next().ok_or(JwtValidationError::Malformed)?;
    let signature = segments.next().ok_or(JwtValidationError::Malformed)?;
    if segments.next().is_some() {
        return Err(JwtValidationError::Malformed);
    }
    let header_value: serde_json::Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(header)
            .map_err(|_| JwtValidationError::Malformed)?,
    )
    .map_err(|_| JwtValidationError::Malformed)?;
    if header_value.get("alg").and_then(|value| value.as_str()) != Some("HS256") {
        return Err(JwtValidationError::UnsupportedAlgorithm);
    }
    let supplied = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| JwtValidationError::Malformed)?;
    let canonical = format!("{header}.{claims}");
    let valid = secrets.iter().any(|secret| {
        Hmac::<Sha256>::new_from_slice(secret)
            .map(|mut mac| {
                mac.update(canonical.as_bytes());
                mac.verify_slice(&supplied).is_ok()
            })
            .unwrap_or(false)
    });
    if !valid {
        return Err(JwtValidationError::InvalidSignature);
    }
    let claim_bytes = URL_SAFE_NO_PAD
        .decode(claims)
        .map_err(|_| JwtValidationError::Malformed)?;
    let claim_value: serde_json::Value =
        serde_json::from_slice(&claim_bytes).map_err(|_| JwtValidationError::InvalidClaims)?;
    if claim_value
        .get("exp")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|expires| now_unix_seconds > expires.saturating_add(leeway_seconds))
    {
        return Err(JwtValidationError::Expired);
    }
    if claim_value
        .get("nbf")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|not_before| now_unix_seconds.saturating_add(leeway_seconds) < not_before)
    {
        return Err(JwtValidationError::NotYetValid);
    }
    serde_json::from_slice(&claim_bytes).map_err(|_| JwtValidationError::InvalidClaims)
}

pub fn encode_jwt_hs256<T>(claims: &T, secret: &[u8]) -> Result<String, JwtValidationError>
where
    T: Serialize,
{
    if secret.is_empty() {
        return Err(JwtValidationError::InvalidSignature);
    }
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).map_err(|_| JwtValidationError::InvalidClaims)?);
    let signing_input = format!("{header}.{claims}");
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret).map_err(|_| JwtValidationError::InvalidSignature)?;
    mac.update(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    ))
}

pub const AUTH_KEY_ID_HEADER: &str = "x-rust-zero-key-id";
pub const AUTH_TIMESTAMP_HEADER: &str = "x-rust-zero-timestamp";
pub const AUTH_SIGNATURE_HEADER: &str = "x-rust-zero-signature";

/// Selects JWT values into handler-facing names using dot-separated payload paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JwtClaimProjection {
    #[serde(default)]
    pub fields: std::collections::BTreeMap<String, String>,
}

impl JwtClaimProjection {
    pub fn new(fields: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            fields: fields.into_iter().collect(),
        }
    }

    pub fn project(
        &self,
        claims: &serde_json::Value,
    ) -> std::collections::BTreeMap<String, serde_json::Value> {
        self.fields
            .iter()
            .filter_map(|(name, path)| {
                path.split('.')
                    .try_fold(claims, |value, segment| value.get(segment))
                    .cloned()
                    .map(|value| (name.clone(), value))
            })
            .collect()
    }
}

/// Stable authentication failures shared by HTTP and gRPC adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    MissingCredentials,
    MalformedCredentials,
    InvalidCredentials,
    ExpiredCredentials,
    NotYetValid,
    MissingSignature,
    InvalidSignature,
    StaleSignature,
}

impl AuthFailure {
    pub const fn code(self) -> &'static str {
        match self {
            Self::MissingCredentials => "auth_missing_credentials",
            Self::MalformedCredentials => "auth_malformed_credentials",
            Self::InvalidCredentials => "auth_invalid_credentials",
            Self::ExpiredCredentials => "auth_expired_credentials",
            Self::NotYetValid => "auth_not_yet_valid",
            Self::MissingSignature => "auth_missing_signature",
            Self::InvalidSignature => "auth_invalid_signature",
            Self::StaleSignature => "auth_stale_signature",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::MissingCredentials => "authentication credentials are required",
            Self::MalformedCredentials => "authentication credentials are malformed",
            Self::InvalidCredentials => "authentication credentials are invalid",
            Self::ExpiredCredentials => "authentication credentials have expired",
            Self::NotYetValid => "authentication credentials are not yet valid",
            Self::MissingSignature => "request signature is required",
            Self::InvalidSignature => "request signature is invalid",
            Self::StaleSignature => "request signature timestamp is outside the allowed window",
        }
    }
}

impl fmt::Display for AuthFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for AuthFailure {}

/// Signature fields transported as HTTP headers or gRPC metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSignature {
    pub key_id: String,
    pub timestamp: i64,
    pub signature: String,
}

/// Signs a transport target using `HMAC-SHA256(timestamp + method + target)`.
///
/// For REST, `target` should be the path and query. For gRPC, it should be the canonical
/// `/package.Service/Method` path. Including the method prevents cross-verb replay.
pub fn sign_request(
    key_id: impl Into<String>,
    secret: &[u8],
    timestamp: i64,
    method: &str,
    target: &str,
) -> Result<RequestSignature, AuthFailure> {
    if secret.is_empty() || method.is_empty() || target.is_empty() {
        return Err(AuthFailure::InvalidSignature);
    }
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret).map_err(|_| AuthFailure::InvalidSignature)?;
    mac.update(canonical(timestamp, method, target).as_bytes());
    Ok(RequestSignature {
        key_id: key_id.into(),
        timestamp,
        signature: URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()),
    })
}

/// Verifies named signing keys and rejects signatures outside a bounded clock-skew window.
#[derive(Clone)]
pub struct RequestSignatureVerifier {
    keys: Arc<HashMap<String, Arc<[u8]>>>,
    max_clock_skew: Duration,
}

impl fmt::Debug for RequestSignatureVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestSignatureVerifier")
            .field("key_ids", &self.keys.keys())
            .field("max_clock_skew", &self.max_clock_skew)
            .finish()
    }
}

impl RequestSignatureVerifier {
    pub fn new(
        keys: impl IntoIterator<Item = (String, Vec<u8>)>,
        max_clock_skew: Duration,
    ) -> Result<Self, AuthFailure> {
        if max_clock_skew.is_zero() {
            return Err(AuthFailure::InvalidSignature);
        }
        let keys: HashMap<_, _> = keys
            .into_iter()
            .map(|(id, secret)| (id, Arc::<[u8]>::from(secret)))
            .collect();
        if keys.is_empty()
            || keys
                .iter()
                .any(|(id, secret)| id.is_empty() || secret.is_empty())
        {
            return Err(AuthFailure::InvalidSignature);
        }
        Ok(Self {
            keys: Arc::new(keys),
            max_clock_skew,
        })
    }

    pub fn verify(
        &self,
        signature: &RequestSignature,
        method: &str,
        target: &str,
        now_unix_seconds: i64,
    ) -> Result<(), AuthFailure> {
        if now_unix_seconds.abs_diff(signature.timestamp) > self.max_clock_skew.as_secs() {
            return Err(AuthFailure::StaleSignature);
        }
        let secret = self
            .keys
            .get(&signature.key_id)
            .ok_or(AuthFailure::InvalidSignature)?;
        let supplied = URL_SAFE_NO_PAD
            .decode(&signature.signature)
            .map_err(|_| AuthFailure::InvalidSignature)?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret).map_err(|_| AuthFailure::InvalidSignature)?;
        mac.update(canonical(signature.timestamp, method, target).as_bytes());
        mac.verify_slice(&supplied)
            .map_err(|_| AuthFailure::InvalidSignature)
    }
}

fn canonical(timestamp: i64, method: &str, target: &str) -> String {
    format!("{timestamp}\n{}\n{target}", method.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_rotation_keys_and_rejects_replay_or_target_changes() {
        let verifier = RequestSignatureVerifier::new(
            [("current".to_owned(), b"secret".to_vec())],
            Duration::from_secs(30),
        )
        .unwrap();
        let signature = sign_request("current", b"secret", 1_000, "post", "/v1/jobs?a=1").unwrap();

        assert_eq!(
            verifier.verify(&signature, "POST", "/v1/jobs?a=1", 1_020),
            Ok(())
        );
        assert_eq!(
            verifier.verify(&signature, "POST", "/v1/jobs?a=2", 1_020),
            Err(AuthFailure::InvalidSignature)
        );
        assert_eq!(
            verifier.verify(&signature, "POST", "/v1/jobs?a=1", 1_031),
            Err(AuthFailure::StaleSignature)
        );
    }
}
