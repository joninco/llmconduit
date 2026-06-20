//! Dashboard + `/debug` access control (D7, stage D7a).
//!
//! This module owns the security foundation for the optional debug UI and the
//! embedded dashboard SPA: env-only secrets, a stateless HMAC-SHA256 signed
//! session cookie, the login/logout handlers, the auth middleware (applied to
//! `/debug`, `/debug/ws`, `/dashboard`, and — later — `/dashboard/api/*`), a
//! WebSocket auth helper (signed cookie + `Origin` allow-list + cookie-`exp`
//! used to close the socket at expiry), and the `MutationPolicy` + CSRF
//! double-submit primitives that D6/D13 consume when wiring the kill route.
//!
//! ## Why secrets are env-only
//! The persisted [`crate::config::Config`] derives `Debug + Clone` and is
//! cloned into every [`crate::engine::Gateway`]. Putting the dashboard token /
//! session-signing key on it would risk leaking them through a `Debug` dump or
//! a config round-trip. Instead [`DashboardAuth`] is built from the process
//! environment ONCE at startup, stored behind an `Arc` in the router's
//! extension layer (NOT on `Config`), and given a hand-written [`std::fmt::Debug`]
//! that never prints the signing key or the token.
//!
//! ## Stateless signed cookie (rotation caveat)
//! The session cookie is `base64url(HMAC-SHA256(key, "{exp}:{nonce}")) +
//! "." + "{exp}:{nonce}"`. Verification recomputes the MAC and compares it in
//! constant time, then checks `exp` has not passed. There is no server-side
//! session table, so: (a) a leaked cookie is valid until its `exp` (≤ 1 h);
//! (b) rotating `LLMCONDUIT_DASHBOARD_SESSION_KEY` invalidates ALL live
//! sessions. Both are documented trade-offs; revocable sessions are future
//! work.
//!
//! ## D7b (deferred)
//! The batched `DashboardFrame` WS envelope, its `DashboardPayload` arms, the
//! `/dashboard/ws` route, and per-domain dedup are NOT in this stage — they
//! depend on the D3/D4/D5 payload types that do not exist yet. See the `// D7b:`
//! markers in `http.rs`/`debug_ui.rs` for the plug-in points.

use axum::Extension;
use axum::Json;
use axum::extract::FromRequestParts;
use axum::extract::OptionalFromRequestParts;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::Hmac;
use hmac::Mac;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use subtle::ConstantTimeEq;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Session lifetime: the cookie `Max-Age` AND the signed-payload `exp` window.
/// 1 hour bounds how long a copied/leaked cookie stays valid (stateless — no
/// server-side revocation).
pub const SESSION_TTL_SECS: u64 = 3600;

/// Session cookie name (signed, `HttpOnly`).
pub const SESSION_COOKIE: &str = "llmconduit_session";
/// Double-submit CSRF cookie name (NON-`HttpOnly` so the SPA can echo it in the
/// `X-CSRF-Token` header).
pub const CSRF_COOKIE: &str = "llmconduit_csrf";
/// Header carrying the double-submit CSRF token on a mutation request.
pub const CSRF_HEADER: &str = "x-csrf-token";

/// Minimum decoded length (bytes) for `LLMCONDUIT_DASHBOARD_SESSION_KEY`.
const MIN_SESSION_KEY_BYTES: usize = 32;

// ---------------------------------------------------------------------------
// Env var names (single authority)
// ---------------------------------------------------------------------------

const ENV_TOKEN: &str = "LLMCONDUIT_DASHBOARD_TOKEN";
const ENV_SESSION_KEY: &str = "LLMCONDUIT_DASHBOARD_SESSION_KEY";
const ENV_PUBLIC_ORIGIN: &str = "LLMCONDUIT_DASHBOARD_PUBLIC_ORIGIN";
const ENV_ALLOW_INSECURE: &str = "LLMCONDUIT_ALLOW_INSECURE_DASHBOARD";
const ENV_ALLOW_MUTATIONS: &str = "LLMCONDUIT_DASHBOARD_ALLOW_MUTATIONS";

// ---------------------------------------------------------------------------
// Environment snapshot (so loading + the startup decision are unit-testable
// without touching the real process environment)
// ---------------------------------------------------------------------------

/// A read-only snapshot of the dashboard-relevant environment. Taking the env
/// as data (rather than reading `std::env` inline) lets every loading/startup
/// decision be exercised deterministically in tests.
#[derive(Debug, Clone, Default)]
pub struct DashboardEnv {
    pub token: Option<String>,
    pub session_key_b64: Option<String>,
    pub public_origin: Option<String>,
    pub allow_insecure: bool,
    pub allow_mutations: bool,
}

impl DashboardEnv {
    /// Read the dashboard env vars from the live process environment.
    pub fn from_process_env() -> Self {
        let read = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        Self {
            token: read(ENV_TOKEN),
            session_key_b64: read(ENV_SESSION_KEY),
            public_origin: read(ENV_PUBLIC_ORIGIN),
            allow_insecure: env_flag(ENV_ALLOW_INSECURE),
            allow_mutations: env_flag(ENV_ALLOW_MUTATIONS),
        }
    }
}

/// A boolean env flag is true only for the explicit affirmative values `1`/
/// `true`/`yes` (case-insensitive). Anything else — including unset — is false,
/// so the secure default (mutations off, TLS required) holds unless explicitly
/// overridden.
fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

// ---------------------------------------------------------------------------
// Origin validation
// ---------------------------------------------------------------------------

/// A validated public origin (`https://host[:port]`, no path/query/fragment).
/// Stored as the exact normalized string we match `Origin` headers against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicOrigin(String);

impl PublicOrigin {
    /// Parse and validate a configured public origin. It MUST be an absolute
    /// `https://` URL with a host and no path (`scheme://host[:port]`). Returns
    /// the normalized `scheme://host[:port]` string on success.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let url = url::Url::parse(raw.trim())
            .map_err(|err| format!("{ENV_PUBLIC_ORIGIN} is not a valid URL: {err}"))?;
        if url.scheme() != "https" {
            return Err(format!(
                "{ENV_PUBLIC_ORIGIN} must use https:// (got {})",
                url.scheme()
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| format!("{ENV_PUBLIC_ORIGIN} has no host"))?;
        if url.path() != "/" && !url.path().is_empty() {
            return Err(format!(
                "{ENV_PUBLIC_ORIGIN} must be an origin only (no path): {raw}"
            ));
        }
        // Re-serialize as a bare origin so an `Origin` header (which never has a
        // trailing slash) compares byte-for-byte.
        let normalized = match url.port() {
            Some(port) => format!("https://{host}:{port}"),
            None => format!("https://{host}"),
        };
        Ok(Self(normalized))
    }

    /// The normalized `https://host[:port]` string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// DashboardAuth — built from env at startup, shared as Arc state
// ---------------------------------------------------------------------------

/// The dashboard/`/debug` auth context: the bearer token, the HMAC signing key,
/// the public-origin allow-list entry, and the mutation toggle. Built once from
/// [`DashboardEnv`] and shared (behind `Arc`) as an Axum extension on the
/// protected routes. NEVER stored on the persisted `Config`.
pub struct DashboardAuth {
    /// Optional bearer/login token. `None` on a loopback dev server without a
    /// configured token — in that mode the login flow always "succeeds"
    /// (the server is already only reachable from localhost). On a non-loopback
    /// bind a token is REQUIRED (enforced by [`startup_route_decision`]).
    token: Option<String>,
    /// HMAC-SHA256 signing key for the session cookie (≥ 32 bytes). Decoded from
    /// base64 once; never logged, never `Debug`-printed.
    session_key: Vec<u8>,
    /// The validated public origin, when configured. Drives the `Secure` cookie
    /// attribute and the WS `Origin` allow-list.
    public_origin: Option<PublicOrigin>,
    /// Whether mutating dashboard routes (the D6 kill route) may proceed.
    /// Default off → mutations are 403.
    allow_mutations: bool,
}

impl std::fmt::Debug for DashboardAuth {
    /// Redacts the signing key and token so a `{:?}` dump never leaks secrets.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DashboardAuth")
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .field("session_key", &"[redacted]")
            .field(
                "public_origin",
                &self.public_origin.as_ref().map(PublicOrigin::as_str),
            )
            .field("allow_mutations", &self.allow_mutations)
            .finish()
    }
}

/// Outcome of building [`DashboardAuth`] from the environment, distinguishing a
/// hard configuration error from a dev-mode concession that should be logged.
/// `Debug` is safe: the inner [`DashboardAuth`] redacts its secrets.
#[derive(Debug)]
pub struct DashboardAuthBuild {
    pub auth: Arc<DashboardAuth>,
    /// Human-readable warnings to log at startup (e.g. an auto-generated key, or
    /// a tokenless loopback dev server). Never contains secret material.
    pub warnings: Vec<String>,
}

impl DashboardAuth {
    /// Build the auth context from an env snapshot for a server binding to
    /// `bind_addr`.
    ///
    /// Rules (mirrors the spec):
    /// - `LLMCONDUIT_DASHBOARD_SESSION_KEY`: must decode to ≥ 32 bytes when set.
    ///   On a loopback bind it may be auto-generated (logged as temporary); on a
    ///   non-loopback bind it is REQUIRED — a missing key fails closed here (no
    ///   silent ephemeral key), independent of [`startup_route_decision`].
    /// - `LLMCONDUIT_DASHBOARD_PUBLIC_ORIGIN`: must be a valid `https://` origin
    ///   when set; required on a non-loopback bind (unless insecure override).
    /// - `LLMCONDUIT_DASHBOARD_TOKEN`: required on a non-loopback bind (the
    ///   insecure override does NOT relax this); optional (dev concession) on
    ///   loopback, where `dev_open` then authenticates every request.
    ///
    /// Returns `Err` for a malformed value (bad base64 / too-short key / bad
    /// origin) OR a missing session key on a non-loopback bind. The
    /// *route-registration* refusal (missing token/key/origin on a non-loopback
    /// bind) is a separate, testable decision — [`startup_route_decision`] — so a
    /// misconfigured production server refuses to expose the routes; this
    /// constructor additionally fails closed so a direct caller cannot obtain an
    /// auto-keyed non-loopback context.
    pub fn from_env(
        bind_addr: SocketAddr,
        env: &DashboardEnv,
    ) -> Result<DashboardAuthBuild, String> {
        let loopback = bind_addr.ip().is_loopback();
        let mut warnings = Vec::new();

        let session_key = match env.session_key_b64.as_deref() {
            Some(encoded) => decode_session_key(encoded)?,
            None if loopback => {
                // No key configured. On loopback we auto-generate an ephemeral
                // one (sessions do not survive a restart — documented).
                warnings.push(format!(
                    "{ENV_SESSION_KEY} not set; generated a temporary loopback-dev signing key \
                     (sessions reset on restart; the key is never logged)"
                ));
                generate_session_key()
            }
            None => {
                // Non-loopback bind with no key: ephemeral auto-generation is a
                // loopback-only concession. Refuse rather than silently signing
                // sessions with a per-process key the operator never set. (The
                // route decision also refuses this, but `from_env` fails closed
                // independently so a direct caller can't get an auto-keyed
                // non-loopback context.)
                return Err(format!(
                    "{ENV_SESSION_KEY} is required on a non-loopback bind (>= {MIN_SESSION_KEY_BYTES} \
                     decoded bytes of base64); ephemeral key generation is loopback-dev only"
                ));
            }
        };

        let public_origin = match env.public_origin.as_deref() {
            Some(raw) => Some(PublicOrigin::parse(raw)?),
            None => None,
        };

        Ok(DashboardAuthBuild {
            auth: Arc::new(Self {
                token: env.token.clone(),
                session_key,
                public_origin,
                allow_mutations: env.allow_mutations,
            }),
            warnings,
        })
    }

    /// Whether the server should send the `Secure` cookie attribute. Tied to a
    /// configured (https) public origin: on a loopback dev server over plain
    /// HTTP, `Secure` cookies would never be stored by the browser.
    pub fn secure_cookies(&self) -> bool {
        self.public_origin.is_some()
    }

    /// The configured public origin, if any.
    pub fn public_origin(&self) -> Option<&PublicOrigin> {
        self.public_origin.as_ref()
    }

    pub fn mutations_enabled(&self) -> bool {
        self.allow_mutations
    }

    /// Constant-time check of a presented login/bearer token against the
    /// configured token. When no token is configured (loopback dev), every
    /// presented token is accepted — the server is only reachable from
    /// localhost in that mode.
    pub fn verify_token(&self, presented: &str) -> bool {
        match self.token.as_deref() {
            None => true,
            Some(expected) => bool::from(presented.as_bytes().ct_eq_padded(expected.as_bytes())),
        }
    }

    // -- session cookie sign/verify ---------------------------------------

    /// Mint a signed session-cookie value with `exp = now + SESSION_TTL_SECS`.
    /// Returns `(cookie_value, exp_unix_secs)`.
    pub fn issue_session(&self) -> (String, u64) {
        let exp = now_unix().saturating_add(SESSION_TTL_SECS);
        (self.sign_session(exp), exp)
    }

    /// Sign a `{exp}:{nonce}` payload, returning the full cookie value
    /// `base64url(mac).{exp}:{nonce}`.
    fn sign_session(&self, exp: u64) -> String {
        let nonce = Uuid::new_v4().simple().to_string();
        let payload = format!("{exp}:{nonce}");
        let mac = self.mac(payload.as_bytes());
        format!("{}.{payload}", URL_SAFE_NO_PAD.encode(mac))
    }

    /// Verify a session-cookie value: split on the FIRST `.`, recompute the MAC
    /// over the payload, compare in constant time, then confirm `exp` is in the
    /// future. Returns the cookie `exp` (unix secs) on success.
    pub fn verify_session(&self, cookie_value: &str) -> Option<u64> {
        let (mac_b64, payload) = cookie_value.split_once('.')?;
        let presented_mac = URL_SAFE_NO_PAD.decode(mac_b64).ok()?;
        let expected_mac = self.mac(payload.as_bytes());
        // Constant-time MAC comparison (length-independent: unequal lengths
        // compare false without early return).
        if !bool::from(presented_mac.ct_eq_padded(&expected_mac)) {
            return None;
        }
        let exp: u64 = payload.split(':').next()?.parse().ok()?;
        if exp <= now_unix() {
            return None;
        }
        Some(exp)
    }

    fn mac(&self, message: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(&self.session_key)
            .expect("HMAC accepts a key of any length");
        mac.update(message);
        mac.finalize().into_bytes().to_vec()
    }

    // -- request authentication -------------------------------------------

    /// Dev-open mode: NO token is configured. This only happens on a loopback
    /// bind (the non-loopback startup decision refuses to register the routes
    /// without a token), where the server is reachable only from localhost. In
    /// this mode every request is treated as authenticated so a developer can
    /// open `/debug`/`/dashboard` without a login round-trip. A logged warning
    /// at startup makes the concession explicit.
    pub fn dev_open(&self) -> bool {
        self.token.is_none()
    }

    /// Authenticate an HTTP request from its headers: dev-open mode (no token
    /// configured → always authenticated), a valid signed session cookie, OR
    /// (non-browser fallback) a constant-time `Authorization: Bearer` token
    /// match. Returns the session `exp` (cookie path) or `u64::MAX` (dev-open /
    /// bearer path — neither has a cookie expiry to track).
    pub fn authenticate(&self, headers: &HeaderMap) -> Option<u64> {
        if self.dev_open() {
            return Some(u64::MAX);
        }
        if let Some(value) = cookie_value(headers, SESSION_COOKIE)
            && let Some(exp) = self.verify_session(&value)
        {
            return Some(exp);
        }
        if let Some(token) = bearer_token(headers)
            && self.verify_token(&token)
        {
            return Some(u64::MAX);
        }
        None
    }

    /// Validate a WebSocket upgrade: (1) a valid signed session cookie and
    /// (2) an `Origin` header on the allow-list (the served origin or the exact
    /// configured `PUBLIC_ORIGIN`). Returns the cookie `exp` so the socket can
    /// be closed when it passes. The bearer fallback is intentionally NOT
    /// honored for WS — browsers cannot set `Authorization` on a `WebSocket`,
    /// so a cookie is the only legitimate browser path and skipping bearer
    /// keeps the CSWSH `Origin` check meaningful.
    pub fn authenticate_ws(&self, headers: &HeaderMap) -> Option<u64> {
        // The Origin allow-list (CSWSH defense) applies even in dev-open mode —
        // a cross-site page must not open the socket regardless of the cookie.
        if !self.origin_allowed(headers) {
            return None;
        }
        if self.dev_open() {
            // No cookie to expire → no per-connection close timer.
            return Some(u64::MAX);
        }
        cookie_value(headers, SESSION_COOKIE).and_then(|value| self.verify_session(&value))
    }

    /// Whether the request's `Origin` is allowed for a WS upgrade. A request
    /// with NO `Origin` (a non-browser client) is allowed — the CSWSH risk is
    /// browser-only, and such a client already passed the signed-cookie check.
    ///
    /// When an `Origin` IS present:
    /// - If a `PUBLIC_ORIGIN` is configured (production), the origin must equal
    ///   it EXACTLY. We deliberately do NOT fall back to the request's `Host`
    ///   here: `Host` is attacker-controllable, so trusting it would let a
    ///   page on `https://evil` with a forged `Host: evil` ride a stolen cookie.
    /// - If NO `PUBLIC_ORIGIN` is configured (loopback/dev), we accept the
    ///   request's own `Host`-derived origin (the served origin), since on a
    ///   localhost bind the `Host` is the loopback address the dev is using.
    fn origin_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
            return true;
        };
        match self.public_origin.as_ref() {
            Some(public) => origin == public.as_str(),
            None => same_origin_as_host(headers, origin),
        }
    }

    // -- CSRF double-submit -----------------------------------------------

    /// Mint a fresh CSRF token for the double-submit cookie + SPA bootstrap.
    pub fn issue_csrf_token(&self) -> String {
        Uuid::new_v4().simple().to_string()
    }

    /// Verify the double-submit CSRF token: the `X-CSRF-Token` header must
    /// constant-time-equal the `llmconduit_csrf` cookie, and neither may be
    /// empty. (The token's unforgeability comes from the `SameSite=Strict`
    /// session cookie gating the request; the double-submit defends the
    /// non-`HttpOnly` cookie against a same-site script that cannot read it.)
    pub fn verify_csrf(&self, headers: &HeaderMap) -> bool {
        let header_token = headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let cookie_token = cookie_value(headers, CSRF_COOKIE).unwrap_or_default();
        if header_token.is_empty() || cookie_token.is_empty() {
            return false;
        }
        bool::from(
            header_token
                .as_bytes()
                .ct_eq_padded(cookie_token.as_bytes()),
        )
    }
}

// ---------------------------------------------------------------------------
// MutationPolicy — pluggable gate D6/D13 consume for the kill route
// ---------------------------------------------------------------------------

/// Decides whether a mutating dashboard action (the D6 flow-kill route) may
/// proceed. D6 compiles/tests against this trait with a mock impl, so D6 has no
/// build dependency on D7 (one-way edge D7→D6, no cycle).
pub trait MutationPolicy: Send + Sync {
    /// Whether mutations are enabled at all (the `ALLOW_MUTATIONS` gate). When
    /// `false`, a mutation route returns 403 before any CSRF work.
    fn mutations_enabled(&self) -> bool;

    /// Authorize a specific mutation request: mutations must be enabled AND the
    /// request must carry a valid double-submit CSRF token.
    fn authorize_mutation(&self, headers: &HeaderMap) -> Result<(), MutationDenied>;
}

/// Why a mutation was refused. Maps to an HTTP status at the route layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationDenied {
    /// Mutations are globally disabled (`ALLOW_MUTATIONS` not set) → 403.
    Disabled,
    /// CSRF double-submit token missing or mismatched → 403.
    CsrfInvalid,
}

impl MutationDenied {
    pub fn status(self) -> StatusCode {
        StatusCode::FORBIDDEN
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::Disabled => "dashboard mutations are disabled",
            Self::CsrfInvalid => "missing or invalid CSRF token",
        }
    }
}

impl MutationPolicy for DashboardAuth {
    fn mutations_enabled(&self) -> bool {
        self.allow_mutations
    }

    fn authorize_mutation(&self, headers: &HeaderMap) -> Result<(), MutationDenied> {
        if !self.allow_mutations {
            return Err(MutationDenied::Disabled);
        }
        if !self.verify_csrf(headers) {
            return Err(MutationDenied::CsrfInvalid);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Startup route-registration decision (pure + testable)
// ---------------------------------------------------------------------------

/// Why the protected routes were refused (for a precise startup log/test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRefusal {
    /// No `LLMCONDUIT_DASHBOARD_TOKEN`. ALWAYS fatal on a non-loopback bind —
    /// the `ALLOW_INSECURE_DASHBOARD` override does NOT relax this (a tokenless
    /// non-loopback dashboard would be fully unauthenticated via `dev_open`).
    MissingToken,
    /// No valid `LLMCONDUIT_DASHBOARD_SESSION_KEY` (missing or < 32 decoded
    /// bytes). ALWAYS fatal on a non-loopback bind — ephemeral key generation is
    /// a loopback-only dev concession.
    MissingSessionKey,
    /// No valid `https://` `LLMCONDUIT_DASHBOARD_PUBLIC_ORIGIN`. Fatal on a
    /// non-loopback bind UNLESS `ALLOW_INSECURE_DASHBOARD=1` relaxes the TLS
    /// requirement.
    MissingHttpsOrigin,
}

impl RouteRefusal {
    pub fn reason(self) -> &'static str {
        match self {
            Self::MissingToken => "LLMCONDUIT_DASHBOARD_TOKEN is required on a non-loopback bind",
            Self::MissingSessionKey => {
                "a valid LLMCONDUIT_DASHBOARD_SESSION_KEY (>= 32 decoded bytes of base64) is \
                 required on a non-loopback bind"
            }
            Self::MissingHttpsOrigin => {
                "a valid https:// LLMCONDUIT_DASHBOARD_PUBLIC_ORIGIN is required on a non-loopback \
                 bind (or set LLMCONDUIT_ALLOW_INSECURE_DASHBOARD=1 to relax ONLY the TLS \
                 requirement)"
            }
        }
    }
}

/// The startup decision for whether the protected routes (`/dashboard`,
/// `/debug`, and their sub-routes) may be registered, given the bind address
/// and the env snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// Register the routes. `warnings` are logged (e.g. a tokenless loopback dev
    /// server, or an insecure-override that bypassed the TLS requirement).
    Register { warnings: Vec<String> },
    /// Refuse to register the protected routes (production misconfiguration).
    Refuse(RouteRefusal),
}

impl RouteDecision {
    pub fn should_register(&self) -> bool {
        matches!(self, Self::Register { .. })
    }
}

/// Decide whether the protected routes may register.
///
/// - **Loopback bind:** always register. A missing token is a dev concession
///   (warned, and `dev_open` authenticates every request — reachable ONLY here);
///   a missing session key is auto-generated; a missing https origin is fine
///   (localhost is not TLS).
/// - **Non-loopback bind:** require a token AND a valid session key AND a valid
///   `https://` public origin. `LLMCONDUIT_ALLOW_INSECURE_DASHBOARD=1` relaxes
///   ONLY the https-origin (TLS) requirement — it does NOT relax the token or
///   session-key requirements. A tokenless non-loopback dashboard would be fully
///   unauthenticated (`dev_open` treats every request as authed), so the token
///   is a hard requirement regardless of the insecure override; the session key
///   is required because ephemeral key generation is a loopback-only concession.
///
/// A *malformed* public origin (bad URL / not https) counts as "missing" here
/// so the routes refuse rather than register with an unusable origin; the
/// precise parse error is surfaced when [`DashboardAuth::from_env`] runs.
pub fn startup_route_decision(bind_addr: SocketAddr, env: &DashboardEnv) -> RouteDecision {
    if bind_addr.ip().is_loopback() {
        let mut warnings = Vec::new();
        if env.token.is_none() {
            warnings.push(format!(
                "{ENV_TOKEN} not set on a loopback dev bind; /dashboard and /debug are served \
                 WITHOUT a login token (localhost only)"
            ));
        }
        return RouteDecision::Register { warnings };
    }

    // Non-loopback. The token and session key are ALWAYS required (the insecure
    // override never relaxes them); the https origin is required UNLESS overridden.
    if env.token.is_none() {
        return RouteDecision::Refuse(RouteRefusal::MissingToken);
    }
    if !has_valid_session_key(env) {
        return RouteDecision::Refuse(RouteRefusal::MissingSessionKey);
    }

    let has_https_origin = env
        .public_origin
        .as_deref()
        .is_some_and(|raw| PublicOrigin::parse(raw).is_ok());
    if has_https_origin {
        return RouteDecision::Register {
            warnings: Vec::new(),
        };
    }

    // Token + key are present, but the https origin is missing/malformed. The
    // override relaxes ONLY this TLS requirement (and only this) — real
    // cookie/token auth is still enforced (`dev_open` is unreachable here
    // because a token is configured).
    if env.allow_insecure {
        return RouteDecision::Register {
            warnings: vec![format!(
                "{ENV_ALLOW_INSECURE}=1 on a non-loopback bind: serving /dashboard and /debug \
                 over plaintext WITHOUT a validated https {ENV_PUBLIC_ORIGIN} — credentials and \
                 transcripts may be exposed in transit (token + session auth are STILL enforced)"
            )],
        };
    }
    RouteDecision::Refuse(RouteRefusal::MissingHttpsOrigin)
}

/// Whether `env` carries a session key that decodes to a valid (≥ 32 byte) HMAC
/// key. Mirrors [`decode_session_key`] so the startup decision refuses a
/// non-loopback bind whose key is missing or too short BEFORE
/// [`DashboardAuth::from_env`] would silently auto-generate an ephemeral one.
fn has_valid_session_key(env: &DashboardEnv) -> bool {
    env.session_key_b64
        .as_deref()
        .is_some_and(|encoded| decode_session_key(encoded).is_ok())
}

// ---------------------------------------------------------------------------
// Login / logout handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub token: String,
}

/// `POST /dashboard/login` — constant-time token check; on success set the
/// signed `HttpOnly; SameSite=Strict[; Secure]; Path=/; Max-Age=3600` session
/// cookie plus the non-`HttpOnly` double-submit CSRF cookie. Response is always
/// `no-store`.
pub async fn dashboard_login(
    Extension(auth): Extension<Arc<DashboardAuth>>,
    payload: Result<Json<LoginRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let token = match payload {
        Ok(Json(body)) => body.token,
        Err(_) => String::new(),
    };
    if !auth.verify_token(&token) {
        return no_store(
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "invalid token"})),
            )
                .into_response(),
        );
    }

    let (session_value, _exp) = auth.issue_session();
    let csrf = auth.issue_csrf_token();
    let secure = auth.secure_cookies();

    let mut response = no_store(
        (
            StatusCode::OK,
            Json(serde_json::json!({"authenticated": true})),
        )
            .into_response(),
    );
    let headers = response.headers_mut();
    append_set_cookie(headers, &session_cookie(&session_value, secure));
    append_set_cookie(headers, &csrf_cookie(&csrf, secure));
    response
}

/// `POST /dashboard/logout` — clear both cookies (stateless; a copied session
/// cookie remains valid until its `exp`).
pub async fn dashboard_logout(Extension(auth): Extension<Arc<DashboardAuth>>) -> Response {
    let secure = auth.secure_cookies();
    let mut response = no_store(StatusCode::NO_CONTENT.into_response());
    let headers = response.headers_mut();
    append_set_cookie(headers, &expire_cookie(SESSION_COOKIE, secure, true));
    append_set_cookie(headers, &expire_cookie(CSRF_COOKIE, secure, false));
    response
}

// ---------------------------------------------------------------------------
// Auth middleware + extractor for the protected HTTP routes
// ---------------------------------------------------------------------------

/// A successful authentication, attached to the request so handlers can read
/// the session `exp` without re-validating.
#[derive(Debug, Clone, Copy)]
pub struct AuthSession {
    /// Session expiry (unix secs); `u64::MAX` for a bearer-authenticated
    /// (non-browser) request.
    pub exp: u64,
}

/// `axum` middleware enforcing a valid session on the protected HTTP routes
/// (`/debug`, `/debug/app.js`, and — registered by D13 — `/dashboard/api/*`).
/// Reads the shared [`DashboardAuth`] from the request extension installed by
/// the auth layer, validates, and either inserts an [`AuthSession`] extension
/// and continues, or returns `401 no-store`. The auth Extension is layered onto
/// the same routes (see `http.rs`), so it is always present here; a missing
/// extension fails closed.
pub async fn require_session(mut request: axum::extract::Request, next: Next) -> Response {
    let Some(auth) = request.extensions().get::<Arc<DashboardAuth>>().cloned() else {
        return unauthorized();
    };
    match auth.authenticate(request.headers()) {
        Some(exp) => {
            request.extensions_mut().insert(AuthSession { exp });
            next.run(request).await
        }
        None => unauthorized(),
    }
}

/// Extractor form of the same check, for handlers that want the session
/// directly (and for the `/dashboard` shell decision). Reads the shared
/// [`DashboardAuth`] from the request extension installed by the auth layer.
impl<S> FromRequestParts<S> for AuthSession
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Prefer an extension inserted by `require_session` (avoids re-verifying
        // the HMAC); fall back to validating the headers directly.
        if let Some(session) = parts.extensions.get::<AuthSession>() {
            return Ok(*session);
        }
        let auth = parts
            .extensions
            .get::<Arc<DashboardAuth>>()
            .cloned()
            .ok_or_else(unauthorized)?;
        auth.authenticate(&parts.headers)
            .map(|exp| AuthSession { exp })
            .ok_or_else(unauthorized)
    }
}

/// Optional variant: `Option<AuthSession>` is `None` for an unauthenticated
/// request instead of short-circuiting with 401. Used by the `/dashboard` route
/// to choose the login shell vs. the SPA without rejecting the request.
impl<S> OptionalFromRequestParts<S> for AuthSession
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        if let Some(session) = parts.extensions.get::<AuthSession>() {
            return Ok(Some(*session));
        }
        let Some(auth) = parts.extensions.get::<Arc<DashboardAuth>>().cloned() else {
            return Ok(None);
        };
        Ok(auth
            .authenticate(&parts.headers)
            .map(|exp| AuthSession { exp }))
    }
}

// ---------------------------------------------------------------------------
// Cookie + header helpers
// ---------------------------------------------------------------------------

/// Build the `Set-Cookie` value for the signed session cookie. `HttpOnly`
/// (no JS access), `SameSite=Strict` (no cross-site send), `Path=/` (so the
/// SAME cookie authorizes `/dashboard` AND `/debug`), `Max-Age=3600`, and
/// `Secure` only when a public https origin is configured.
fn session_cookie(value: &str, secure: bool) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE}={value}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Build the `Set-Cookie` value for the double-submit CSRF cookie. NON-`HttpOnly`
/// so the SPA can read it and echo it in `X-CSRF-Token`; same `SameSite=Strict`,
/// `Path=/`, `Max-Age`, and `Secure` policy as the session cookie.
fn csrf_cookie(value: &str, secure: bool) -> String {
    let mut cookie =
        format!("{CSRF_COOKIE}={value}; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Build a cookie-clearing `Set-Cookie` (empty value, `Max-Age=0`). `http_only`
/// matches the original cookie's flag so the attributes line up.
fn expire_cookie(name: &str, secure: bool, http_only: bool) -> String {
    let mut cookie = format!("{name}=; SameSite=Strict; Path=/; Max-Age=0");
    if http_only {
        cookie.push_str("; HttpOnly");
    }
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Append a `Set-Cookie` header (multiple cookies → multiple headers).
fn append_set_cookie(headers: &mut HeaderMap, cookie: &str) {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        headers.append(header::SET_COOKIE, value);
    }
}

/// A `401 Unauthorized` with `Cache-Control: no-store`.
fn unauthorized() -> Response {
    no_store((StatusCode::UNAUTHORIZED, "unauthorized").into_response())
}

/// Stamp `Cache-Control: no-store` on a response (auth responses must not be
/// cached by any intermediary).
pub fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Extract a single cookie value by name from the `Cookie` header. Splits on
/// `;`, trims, and matches the `name=` prefix exactly.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name)
            && let Some(value) = rest.strip_prefix('=')
        {
            return Some(value.to_string());
        }
    }
    None
}

/// Extract a bearer token from `Authorization: Bearer <token>` (case-insensitive
/// scheme).
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let header = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = header.strip_prefix("Bearer ").or_else(|| {
        header
            .get(..7)
            .filter(|prefix| prefix.eq_ignore_ascii_case("bearer "))
            .map(|_| &header[7..])
    })?;
    let token = rest.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Whether `origin` equals the request's own `scheme://host[:port]`, derived
/// from `Host` + a `wss`/`https` hint. We can't see the TLS state of the inbound
/// connection from the handler, so we accept either the http or https form of
/// the request's `Host` as "same origin" — this is the LOOPBACK/served-origin
/// path; the strict cross-site defense is the exact `PUBLIC_ORIGIN` match plus
/// `SameSite=Strict` on the cookie.
fn same_origin_as_host(headers: &HeaderMap, origin: &str) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    origin == format!("http://{host}") || origin == format!("https://{host}")
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

/// Seconds since the Unix epoch (monotonic enough for an expiry check; a clock
/// that jumps backwards only shortens a session).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Decode + length-check the configured base64 session key.
fn decode_session_key(encoded: &str) -> Result<Vec<u8>, String> {
    // Accept both standard and URL-safe base64 (with or without padding) so an
    // operator pasting either form works.
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded))
        .or_else(|_| URL_SAFE_NO_PAD.decode(encoded))
        .map_err(|err| format!("{ENV_SESSION_KEY} is not valid base64: {err}"))?;
    if decoded.len() < MIN_SESSION_KEY_BYTES {
        return Err(format!(
            "{ENV_SESSION_KEY} must decode to at least {MIN_SESSION_KEY_BYTES} bytes (got {})",
            decoded.len()
        ));
    }
    Ok(decoded)
}

/// Generate a 32-byte signing key from UUID entropy (no extra RNG dependency).
fn generate_session_key() -> Vec<u8> {
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(Uuid::new_v4().as_bytes());
    key.extend_from_slice(Uuid::new_v4().as_bytes());
    key
}

/// Constant-time, **length-independent** byte-slice equality. Used for the
/// token, session MAC, and CSRF comparisons — all of which may compare slices
/// of differing length (a presented token/cookie is attacker-sized).
///
/// `subtle::ConstantTimeEq` on `[u8]` requires equal lengths and would itself
/// short-circuit (leaking length via timing) on a mismatch. Instead we hash
/// BOTH sides to a fixed 32-byte SHA-256 digest and compare the digests in
/// constant time, then fold in a constant-time length-equality bit. Because the
/// comparison always runs over two fixed-width digests, the work is independent
/// of either input's length, and there is NO early return / branch on the
/// secret-dependent comparison: the length bit is combined with a bitwise `&`
/// on the `subtle::Choice`, which is constant-time. Hashing also means an
/// attacker observing timing learns nothing about the secret's contents
/// (digests of unequal inputs differ pseudo-randomly).
trait CtEqPadded {
    fn ct_eq_padded(&self, other: &[u8]) -> subtle::Choice;
}

impl CtEqPadded for [u8] {
    fn ct_eq_padded(&self, other: &[u8]) -> subtle::Choice {
        // Fixed-width digests → the constant-time compare runs over 32 bytes
        // regardless of input length (no length-dependent work, no early
        // return). SHA-256 is a fixed pre-comparison transform on both sides.
        let lhs = Sha256::digest(self);
        let rhs = Sha256::digest(other);
        // Length-equality is folded in (constant-time `&`) so distinct-length
        // inputs that happen to collide in digest still compare not-equal,
        // WITHOUT branching on `len()` before the compare.
        let len_eq = (self.len() as u64).ct_eq(&(other.len() as u64));
        lhs.ct_eq(rhs.as_slice()) & len_eq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderName;

    fn loopback() -> SocketAddr {
        "127.0.0.1:4000".parse().unwrap()
    }

    fn public_bind() -> SocketAddr {
        "0.0.0.0:4000".parse().unwrap()
    }

    fn key_b64() -> String {
        base64::engine::general_purpose::STANDARD.encode([7u8; 32])
    }

    fn env_with_token() -> DashboardEnv {
        DashboardEnv {
            token: Some("s3cret-token".to_string()),
            session_key_b64: Some(key_b64()),
            public_origin: Some("https://dash.example.com".to_string()),
            allow_insecure: false,
            allow_mutations: false,
        }
    }

    fn build(bind: SocketAddr, env: &DashboardEnv) -> Arc<DashboardAuth> {
        DashboardAuth::from_env(bind, env).unwrap().auth
    }

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    // -- startup route decision -------------------------------------------

    #[test]
    fn non_loopback_without_token_refuses_routes() {
        let env = DashboardEnv {
            token: None,
            public_origin: Some("https://dash.example.com".to_string()),
            ..Default::default()
        };
        let decision = startup_route_decision(public_bind(), &env);
        assert_eq!(decision, RouteDecision::Refuse(RouteRefusal::MissingToken));
        assert!(!decision.should_register());
    }

    #[test]
    fn non_loopback_without_https_origin_refuses_routes() {
        // Token + key present so the https-origin check is the one that fires.
        let env = DashboardEnv {
            token: Some("t".to_string()),
            session_key_b64: Some(key_b64()),
            public_origin: None,
            ..Default::default()
        };
        let decision = startup_route_decision(public_bind(), &env);
        assert_eq!(
            decision,
            RouteDecision::Refuse(RouteRefusal::MissingHttpsOrigin)
        );
    }

    #[test]
    fn non_loopback_with_http_origin_is_treated_as_missing() {
        // A non-https origin is not a valid PUBLIC_ORIGIN → refuse (token + key
        // present so the origin check is reached).
        let env = DashboardEnv {
            token: Some("t".to_string()),
            session_key_b64: Some(key_b64()),
            public_origin: Some("http://dash.example.com".to_string()),
            ..Default::default()
        };
        let decision = startup_route_decision(public_bind(), &env);
        assert_eq!(
            decision,
            RouteDecision::Refuse(RouteRefusal::MissingHttpsOrigin)
        );
    }

    #[test]
    fn non_loopback_bare_env_refuses_on_token_first() {
        // With nothing configured, the token is the first (and unrelaxable)
        // requirement checked → MissingToken (not origin).
        let env = DashboardEnv::default();
        let decision = startup_route_decision(public_bind(), &env);
        assert_eq!(decision, RouteDecision::Refuse(RouteRefusal::MissingToken));
    }

    #[test]
    fn non_loopback_with_token_but_no_session_key_refuses() {
        // Token + https origin present but NO session key → refuse (ephemeral
        // key generation is loopback-only; the spec requires the key here).
        let env = DashboardEnv {
            token: Some("t".to_string()),
            session_key_b64: None,
            public_origin: Some("https://dash.example.com".to_string()),
            ..Default::default()
        };
        let decision = startup_route_decision(public_bind(), &env);
        assert_eq!(
            decision,
            RouteDecision::Refuse(RouteRefusal::MissingSessionKey)
        );
    }

    #[test]
    fn non_loopback_with_short_session_key_refuses() {
        // A present-but-too-short key is as good as missing for the predicate.
        let env = DashboardEnv {
            token: Some("t".to_string()),
            session_key_b64: Some(base64::engine::general_purpose::STANDARD.encode([1u8; 16])),
            public_origin: Some("https://dash.example.com".to_string()),
            ..Default::default()
        };
        let decision = startup_route_decision(public_bind(), &env);
        assert_eq!(
            decision,
            RouteDecision::Refuse(RouteRefusal::MissingSessionKey)
        );
    }

    // -- insecure override: relaxes ONLY https, never the token/key -----------

    #[test]
    fn insecure_override_does_not_relax_missing_token() {
        // ALLOW_INSECURE=1 on a non-loopback bind with NO token must STILL refuse:
        // a tokenless dashboard is `dev_open` (every request authed) — an
        // unauthenticated dashboard on a LAN. The override only relaxes TLS.
        let env = DashboardEnv {
            token: None,
            session_key_b64: Some(key_b64()),
            public_origin: None,
            allow_insecure: true,
            ..Default::default()
        };
        let decision = startup_route_decision(public_bind(), &env);
        assert_eq!(
            decision,
            RouteDecision::Refuse(RouteRefusal::MissingToken),
            "insecure override must NOT register a tokenless non-loopback dashboard"
        );
        assert!(!decision.should_register());
    }

    #[test]
    fn insecure_override_does_not_relax_missing_session_key() {
        // ALLOW_INSECURE=1 + token but NO session key → still refuse (key is not
        // a TLS concern; only the https origin is relaxed).
        let env = DashboardEnv {
            token: Some("t".to_string()),
            session_key_b64: None,
            public_origin: None,
            allow_insecure: true,
            ..Default::default()
        };
        let decision = startup_route_decision(public_bind(), &env);
        assert_eq!(
            decision,
            RouteDecision::Refuse(RouteRefusal::MissingSessionKey)
        );
    }

    #[test]
    fn insecure_override_relaxes_only_https_origin() {
        // ALLOW_INSECURE=1 + token + valid key + NO https origin → register with
        // a warning. `dev_open` is NOT active because a token is configured:
        // real cookie/token auth is enforced, so an unauthenticated request 401s.
        let env = DashboardEnv {
            token: Some("t".to_string()),
            session_key_b64: Some(key_b64()),
            public_origin: None,
            allow_insecure: true,
            ..Default::default()
        };
        let decision = startup_route_decision(public_bind(), &env);
        match &decision {
            RouteDecision::Register { warnings } => {
                assert!(!warnings.is_empty(), "insecure override must warn");
            }
            RouteDecision::Refuse(r) => panic!("expected register, got refuse: {r:?}"),
        }
        // The built auth context enforces real auth (token set → not dev-open),
        // so an unauthenticated request is rejected.
        let auth = build(public_bind(), &env);
        assert!(!auth.dev_open(), "a configured token must disable dev-open");
        assert!(
            auth.authenticate(&HeaderMap::new()).is_none(),
            "no cookie/bearer → unauthenticated even under the insecure override"
        );
    }

    #[test]
    fn non_loopback_with_token_and_https_origin_registers() {
        let decision = startup_route_decision(public_bind(), &env_with_token());
        assert!(decision.should_register());
    }

    #[test]
    fn insecure_override_registers_with_warning_when_token_and_key_present() {
        // The override's legitimate use: token + valid key present, only the
        // https origin relaxed (air-gapped LAN). It registers WITH a warning.
        // (A tokenless/keyless override is covered by the dedicated
        // `insecure_override_does_not_relax_*` tests — those must REFUSE.)
        let env = DashboardEnv {
            token: Some("t".to_string()),
            session_key_b64: Some(key_b64()),
            public_origin: None,
            allow_insecure: true,
            ..Default::default()
        };
        let decision = startup_route_decision(public_bind(), &env);
        match decision {
            RouteDecision::Register { warnings } => {
                assert!(!warnings.is_empty(), "insecure override must warn");
            }
            RouteDecision::Refuse(_) => panic!("insecure override should register"),
        }
    }

    #[test]
    fn loopback_without_token_registers_with_warning() {
        let env = DashboardEnv::default();
        let decision = startup_route_decision(loopback(), &env);
        match decision {
            RouteDecision::Register { warnings } => {
                assert!(!warnings.is_empty(), "tokenless loopback must warn");
            }
            RouteDecision::Refuse(_) => panic!("loopback dev should register"),
        }
    }

    #[test]
    fn loopback_with_token_registers_without_warning() {
        let env = env_with_token();
        let decision = startup_route_decision(loopback(), &env);
        assert_eq!(decision, RouteDecision::Register { warnings: vec![] });
    }

    // -- key + origin validation ------------------------------------------

    #[test]
    fn short_session_key_is_rejected() {
        let env = DashboardEnv {
            session_key_b64: Some(base64::engine::general_purpose::STANDARD.encode([1u8; 16])),
            ..env_with_token()
        };
        let err = DashboardAuth::from_env(loopback(), &env).unwrap_err();
        assert!(err.contains("at least 32 bytes"), "got: {err}");
    }

    #[test]
    fn loopback_auto_generates_key_and_warns() {
        let env = DashboardEnv {
            session_key_b64: None,
            ..Default::default()
        };
        let build = DashboardAuth::from_env(loopback(), &env).unwrap();
        assert!(
            build.warnings.iter().any(|w| w.contains("temporary")),
            "expected a temporary-key warning"
        );
        // The auto-generated key still signs/verifies.
        let (cookie, _) = build.auth.issue_session();
        assert!(build.auth.verify_session(&cookie).is_some());
    }

    #[test]
    fn bad_public_origin_is_rejected() {
        let env = DashboardEnv {
            public_origin: Some("not a url".to_string()),
            ..env_with_token()
        };
        assert!(DashboardAuth::from_env(loopback(), &env).is_err());
    }

    #[test]
    fn public_origin_normalizes_to_bare_origin() {
        assert_eq!(
            PublicOrigin::parse("https://dash.example.com/")
                .unwrap()
                .as_str(),
            "https://dash.example.com"
        );
        assert_eq!(
            PublicOrigin::parse("https://dash.example.com:8443")
                .unwrap()
                .as_str(),
            "https://dash.example.com:8443"
        );
        assert!(PublicOrigin::parse("https://dash.example.com/path").is_err());
        assert!(PublicOrigin::parse("http://dash.example.com").is_err());
    }

    #[test]
    fn debug_redacts_secrets() {
        let auth = build(loopback(), &env_with_token());
        let rendered = format!("{auth:?}");
        assert!(rendered.contains("[redacted]"));
        assert!(!rendered.contains("s3cret-token"));
        assert!(!rendered.contains(&key_b64()));
    }

    // -- token compare -----------------------------------------------------

    #[test]
    fn token_compare_is_exact() {
        let auth = build(public_bind(), &env_with_token());
        assert!(auth.verify_token("s3cret-token"));
        assert!(!auth.verify_token("s3cret-toke"));
        assert!(!auth.verify_token("s3cret-tokenX"));
        assert!(!auth.verify_token("wrong"));
        assert!(!auth.verify_token(""));
    }

    #[test]
    fn tokenless_loopback_accepts_any_token() {
        let env = DashboardEnv {
            token: None,
            ..Default::default()
        };
        let auth = build(loopback(), &env);
        assert!(auth.verify_token("anything"));
    }

    // -- length-independent constant-time compare -------------------------

    #[test]
    fn ct_eq_padded_equal_is_true() {
        assert!(bool::from(b"abcdef".ct_eq_padded(b"abcdef")));
        // Empty == empty.
        assert!(bool::from(b"".ct_eq_padded(b"")));
        // Works at the HMAC width we compare in practice.
        let a = [7u8; 32];
        assert!(bool::from(a.ct_eq_padded(&[7u8; 32])));
    }

    #[test]
    fn ct_eq_padded_unequal_same_length_is_false() {
        assert!(!bool::from(b"abcdef".ct_eq_padded(b"abcdeg")));
        let mut b = [7u8; 32];
        b[31] = 8;
        assert!(!bool::from([7u8; 32].ct_eq_padded(&b)));
    }

    #[test]
    fn ct_eq_padded_different_length_is_false() {
        // A prefix/suffix relationship must NOT compare equal, and the length
        // bit is folded in even when one side is empty.
        assert!(!bool::from(b"abc".ct_eq_padded(b"abcdef")));
        assert!(!bool::from(b"abcdef".ct_eq_padded(b"abc")));
        assert!(!bool::from(b"".ct_eq_padded(b"x")));
        assert!(!bool::from(b"x".ct_eq_padded(b"")));
    }

    #[test]
    fn token_compare_rejects_length_mismatch_via_ct() {
        // Drives the token path (which routes through ct_eq_padded): a shorter
        // and a longer presentation are both rejected without leaking via an
        // early length branch.
        let auth = build(public_bind(), &env_with_token());
        assert!(!auth.verify_token("s3cret-tok")); // shorter
        assert!(!auth.verify_token("s3cret-token-extra")); // longer
    }

    // -- session cookie sign/verify ---------------------------------------

    #[test]
    fn session_roundtrips() {
        let auth = build(public_bind(), &env_with_token());
        let (cookie, exp) = auth.issue_session();
        let verified = auth.verify_session(&cookie).expect("valid cookie verifies");
        assert_eq!(verified, exp);
    }

    #[test]
    fn tampered_session_mac_is_rejected() {
        let auth = build(public_bind(), &env_with_token());
        let (cookie, _) = auth.issue_session();
        let (_, payload) = cookie.split_once('.').unwrap();
        // Forge a MAC over the same payload.
        let forged = format!("{}.{payload}", URL_SAFE_NO_PAD.encode([0u8; 32]));
        assert!(auth.verify_session(&forged).is_none());
    }

    #[test]
    fn cross_key_session_is_rejected() {
        let auth_a = build(public_bind(), &env_with_token());
        let env_b = DashboardEnv {
            session_key_b64: Some(base64::engine::general_purpose::STANDARD.encode([9u8; 32])),
            ..env_with_token()
        };
        let auth_b = build(public_bind(), &env_b);
        let (cookie, _) = auth_a.issue_session();
        assert!(auth_b.verify_session(&cookie).is_none());
    }

    #[test]
    fn expired_session_is_rejected() {
        let auth = build(public_bind(), &env_with_token());
        // Sign with an exp in the past.
        let cookie = auth.sign_session(now_unix().saturating_sub(10));
        assert!(auth.verify_session(&cookie).is_none());
    }

    #[test]
    fn malformed_session_values_are_rejected() {
        let auth = build(public_bind(), &env_with_token());
        assert!(auth.verify_session("").is_none());
        assert!(auth.verify_session("no-dot").is_none());
        assert!(auth.verify_session(".onlypayload").is_none());
        assert!(auth.verify_session("notbase64!.123:abc").is_none());
    }

    // -- request authentication -------------------------------------------

    #[test]
    fn valid_cookie_authenticates() {
        let auth = build(public_bind(), &env_with_token());
        let (cookie, exp) = auth.issue_session();
        let headers = headers_with(&[("cookie", &format!("{SESSION_COOKIE}={cookie}"))]);
        assert_eq!(auth.authenticate(&headers), Some(exp));
    }

    #[test]
    fn missing_cookie_does_not_authenticate() {
        let auth = build(public_bind(), &env_with_token());
        assert!(auth.authenticate(&HeaderMap::new()).is_none());
    }

    #[test]
    fn bearer_fallback_authenticates_with_configured_token() {
        let auth = build(public_bind(), &env_with_token());
        let headers = headers_with(&[("authorization", "Bearer s3cret-token")]);
        assert_eq!(auth.authenticate(&headers), Some(u64::MAX));
        let bad = headers_with(&[("authorization", "Bearer nope")]);
        assert!(auth.authenticate(&bad).is_none());
    }

    #[test]
    fn cookie_is_selected_among_multiple_cookies() {
        let auth = build(public_bind(), &env_with_token());
        let (cookie, _) = auth.issue_session();
        let headers = headers_with(&[(
            "cookie",
            &format!("other=1; {SESSION_COOKIE}={cookie}; {CSRF_COOKIE}=abc"),
        )]);
        assert!(auth.authenticate(&headers).is_some());
    }

    // -- WS auth (cookie + origin) ----------------------------------------

    #[test]
    fn ws_accepts_valid_cookie_and_public_origin() {
        let auth = build(public_bind(), &env_with_token());
        let (cookie, exp) = auth.issue_session();
        let headers = headers_with(&[
            ("cookie", &format!("{SESSION_COOKIE}={cookie}")),
            ("origin", "https://dash.example.com"),
        ]);
        assert_eq!(auth.authenticate_ws(&headers), Some(exp));
    }

    #[test]
    fn ws_accepts_valid_cookie_and_same_host_origin() {
        let env = DashboardEnv {
            public_origin: None,
            ..env_with_token()
        };
        let auth = build(loopback(), &env);
        let (cookie, _) = auth.issue_session();
        let headers = headers_with(&[
            ("cookie", &format!("{SESSION_COOKIE}={cookie}")),
            ("origin", "http://127.0.0.1:4000"),
            ("host", "127.0.0.1:4000"),
        ]);
        assert!(auth.authenticate_ws(&headers).is_some());
    }

    #[test]
    fn ws_rejects_cross_origin() {
        let auth = build(public_bind(), &env_with_token());
        let (cookie, _) = auth.issue_session();
        let headers = headers_with(&[
            ("cookie", &format!("{SESSION_COOKIE}={cookie}")),
            ("origin", "https://evil.example.com"),
            ("host", "dash.example.com"),
        ]);
        assert!(auth.authenticate_ws(&headers).is_none());
    }

    #[test]
    fn ws_with_public_origin_ignores_attacker_host_header() {
        // With a configured PUBLIC_ORIGIN, a forged `Host` matching a malicious
        // `Origin` must NOT be accepted (Host is attacker-controllable).
        let auth = build(public_bind(), &env_with_token());
        let (cookie, _) = auth.issue_session();
        let headers = headers_with(&[
            ("cookie", &format!("{SESSION_COOKIE}={cookie}")),
            ("origin", "https://evil.example.com"),
            ("host", "evil.example.com"),
        ]);
        assert!(auth.authenticate_ws(&headers).is_none());
    }

    #[test]
    fn ws_rejects_bad_cookie_even_with_good_origin() {
        let auth = build(public_bind(), &env_with_token());
        let headers = headers_with(&[
            ("cookie", &format!("{SESSION_COOKIE}=forged.0:0")),
            ("origin", "https://dash.example.com"),
        ]);
        assert!(auth.authenticate_ws(&headers).is_none());
    }

    #[test]
    fn ws_does_not_honor_bearer_fallback() {
        // A browser WebSocket can't set Authorization; bearer must NOT bypass
        // the Origin check on the WS path.
        let auth = build(public_bind(), &env_with_token());
        let headers = headers_with(&[
            ("authorization", "Bearer s3cret-token"),
            ("origin", "https://evil.example.com"),
        ]);
        assert!(auth.authenticate_ws(&headers).is_none());
    }

    // -- CSRF + mutation policy -------------------------------------------

    #[test]
    fn mutations_disabled_by_default() {
        let auth = build(public_bind(), &env_with_token());
        assert!(!auth.mutations_enabled());
        let csrf = auth.issue_csrf_token();
        let headers = headers_with(&[
            ("x-csrf-token", &csrf),
            ("cookie", &format!("{CSRF_COOKIE}={csrf}")),
        ]);
        assert_eq!(
            auth.authorize_mutation(&headers),
            Err(MutationDenied::Disabled)
        );
    }

    #[test]
    fn mutation_requires_matching_csrf_when_enabled() {
        let env = DashboardEnv {
            allow_mutations: true,
            ..env_with_token()
        };
        let auth = build(public_bind(), &env);
        assert!(auth.mutations_enabled());

        let csrf = auth.issue_csrf_token();
        let ok = headers_with(&[
            ("x-csrf-token", &csrf),
            ("cookie", &format!("{CSRF_COOKIE}={csrf}")),
        ]);
        assert_eq!(auth.authorize_mutation(&ok), Ok(()));

        // Mismatched header vs cookie.
        let mismatched = headers_with(&[
            ("x-csrf-token", "different"),
            ("cookie", &format!("{CSRF_COOKIE}={csrf}")),
        ]);
        assert_eq!(
            auth.authorize_mutation(&mismatched),
            Err(MutationDenied::CsrfInvalid)
        );

        // Missing header.
        let no_header = headers_with(&[("cookie", &format!("{CSRF_COOKIE}={csrf}"))]);
        assert_eq!(
            auth.authorize_mutation(&no_header),
            Err(MutationDenied::CsrfInvalid)
        );

        // Empty tokens never validate.
        let empty = headers_with(&[("x-csrf-token", ""), ("cookie", &format!("{CSRF_COOKIE}="))]);
        assert_eq!(
            auth.authorize_mutation(&empty),
            Err(MutationDenied::CsrfInvalid)
        );
    }

    // -- cookie attributes -------------------------------------------------

    #[test]
    fn session_cookie_has_required_attributes_with_secure() {
        let cookie = session_cookie("abc.def", true);
        assert!(cookie.starts_with(&format!("{SESSION_COOKIE}=abc.def")));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains(&format!("Max-Age={SESSION_TTL_SECS}")));
        assert!(cookie.contains("Secure"));
    }

    #[test]
    fn session_cookie_omits_secure_without_public_origin() {
        let cookie = session_cookie("abc.def", false);
        assert!(!cookie.contains("Secure"));
        assert!(cookie.contains("HttpOnly"));
    }

    #[test]
    fn csrf_cookie_is_not_http_only() {
        let cookie = csrf_cookie("tok", true);
        assert!(!cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Path=/"));
    }
}
