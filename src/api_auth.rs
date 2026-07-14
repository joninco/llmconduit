//! Environment-only authentication for the public `/v1/*` API surface.

use axum::http::{HeaderMap, header};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use subtle::{Choice, ConstantTimeEq};

pub const ENV_API_TOKEN: &str = "LLMCONDUIT_API_TOKEN";
pub const ENV_ALLOW_UNAUTHENTICATED: &str = "LLMCONDUIT_ALLOW_UNAUTHENTICATED_API";

#[derive(Clone, Default)]
pub struct ApiAuthEnv {
    pub token: Option<String>,
    pub allow_unauthenticated: bool,
}

impl ApiAuthEnv {
    pub fn from_process_env() -> Self {
        let token = std::env::var(ENV_API_TOKEN)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let allow_unauthenticated = matches!(
            std::env::var(ENV_ALLOW_UNAUTHENTICATED)
                .ok()
                .map(|value| value.trim().to_ascii_lowercase())
                .as_deref(),
            Some("1") | Some("true") | Some("yes")
        );
        Self {
            token,
            allow_unauthenticated,
        }
    }
}

#[derive(Clone)]
pub struct ApiAuth {
    expected_digest: [u8; 32],
}

impl std::fmt::Debug for ApiAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApiAuth")
            .field("configured", &true)
            .finish()
    }
}

impl ApiAuth {
    pub fn new(token: &str) -> Self {
        Self {
            expected_digest: Sha256::digest(token.as_bytes()).into(),
        }
    }

    pub fn from_env(env: &ApiAuthEnv) -> Option<Self> {
        env.token.as_deref().map(Self::new)
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> bool {
        let bearer_match = self.matches(bearer_token(headers));
        let api_key_match = self.matches(
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        );

        // Evaluate both credential channels before combining the Choices. An
        // invalid Authorization header must not mask a valid x-api-key (and the
        // comparison of one channel must not short-circuit the other).
        bool::from(bearer_match | api_key_match)
    }

    fn matches(&self, presented: Option<&str>) -> Choice {
        let Some(presented) = presented else {
            return Choice::from(0);
        };
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        digest.ct_eq(&self.expected_digest)
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

/// Enforce the secure startup rule. A configured token is valid on every bind;
/// unauthenticated serving is limited to loopback unless explicitly overridden.
pub fn validate_startup(bind_addr: SocketAddr, env: &ApiAuthEnv) -> Result<(), String> {
    if env.token.is_some() || bind_addr.ip().is_loopback() || env.allow_unauthenticated {
        return Ok(());
    }
    Err(format!(
        "refusing unauthenticated API on non-loopback bind; set {ENV_API_TOKEN} or explicitly set {ENV_ALLOW_UNAUTHENTICATED}=1"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn loopback_is_allowed_without_token() {
        assert!(
            validate_startup("127.0.0.1:5022".parse().unwrap(), &ApiAuthEnv::default()).is_ok()
        );
    }

    #[test]
    fn public_bind_requires_token_or_explicit_override() {
        let bind = "0.0.0.0:5022".parse().unwrap();
        assert!(validate_startup(bind, &ApiAuthEnv::default()).is_err());
        assert!(
            validate_startup(
                bind,
                &ApiAuthEnv {
                    token: None,
                    allow_unauthenticated: true,
                }
            )
            .is_ok()
        );
        assert!(
            validate_startup(
                bind,
                &ApiAuthEnv {
                    token: Some("dedicated-token".to_string()),
                    allow_unauthenticated: false,
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn bearer_and_x_api_key_authenticate() {
        let auth = ApiAuth::new("dedicated-token");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer dedicated-token"),
        );
        assert!(auth.authenticate(&headers));

        headers.remove(header::AUTHORIZATION);
        headers.insert("x-api-key", HeaderValue::from_static("dedicated-token"));
        assert!(auth.authenticate(&headers));
        headers.insert("x-api-key", HeaderValue::from_static("wrong"));
        assert!(!auth.authenticate(&headers));
    }

    #[test]
    fn either_valid_credential_authenticates_when_both_are_present() {
        let auth = ApiAuth::new("dedicated-token");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("dedicated-token"));
        assert!(auth.authenticate(&headers));

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer dedicated-token"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("wrong"));
        assert!(auth.authenticate(&headers));

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        assert!(!auth.authenticate(&headers));
    }
}
