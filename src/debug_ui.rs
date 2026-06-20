//! `/debug` HTML + `/debug/app.js` + `/debug/ws` handlers.
//!
//! D7: the inline module script that used to live in `debug.html` is now served
//! from `/debug/app.js` (see [`debug_app_js`]) so `/debug` can ship a strict
//! Content-Security-Policy (`script-src 'self'`, no `'unsafe-inline'`). The
//! `/debug` + `/debug/ws` routes are gated behind the dashboard session cookie
//! by the auth layer wired in `http.rs`; the per-connection `Origin` check and
//! cookie-`exp` socket close live in [`debug_ws`]/[`debug_socket`]. The
//! `/debug/ws` WIRE CONTRACT is unchanged — it still streams bare
//! [`DebugWsMessage`] frames (D7b adds the dashboard-only batched envelope on a
//! separate `/dashboard/ws` route).

use crate::engine::Gateway;
use crate::monitor::DebugWsMessage;
use axum::extract::State;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::broadcast;

const DEBUG_HTML: &str = include_str!("debug.html");
/// The dashboard/debug client logic, externalized from `debug.html` so `/debug`
/// can use `script-src 'self'` (D7). Served verbatim at `/debug/app.js`.
const DEBUG_APP_JS: &str = include_str!("debug_app.js");

/// Strict CSP for `/debug`: same shape as the `/dashboard` policy. With the
/// script externalized to `/debug/app.js`, `script-src 'self'` needs NO
/// `'unsafe-inline'`. Styles remain inline (`<style>` in `debug.html`), so
/// `style-src` keeps `'unsafe-inline'`. `connect-src` allows the WS upgrade.
pub const DEBUG_CSP: &str = "default-src 'self'; script-src 'self'; connect-src 'self' ws: wss:; \
     style-src 'self' 'unsafe-inline'; img-src 'self' data:; object-src 'none'; \
     base-uri 'self'; frame-ancestors 'none'";

/// `GET /debug` — the debug UI shell. Carries the strict CSP + the standard
/// security headers + `no-store` (transcripts must not be cached).
pub async fn debug_index() -> Response {
    let mut response = (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DEBUG_HTML,
    )
        .into_response();
    apply_debug_security_headers(response.headers_mut());
    response
}

/// `GET /debug/app.js` — the externalized client module. Same security headers
/// so the JS is served under the same hardened policy.
pub async fn debug_app_js() -> Response {
    let mut response = (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        DEBUG_APP_JS,
    )
        .into_response();
    apply_debug_security_headers(response.headers_mut());
    response
}

/// Apply `Content-Security-Policy`, `X-Frame-Options: DENY`, `nosniff`,
/// `no-referrer`, and `Cache-Control: no-store` to a `/debug` response.
fn apply_debug_security_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(DEBUG_CSP),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

/// `GET /debug/ws` — the monitor WebSocket. The HTTP-layer auth middleware has
/// already validated the session cookie for this path; here we additionally
/// enforce the WS `Origin` allow-list (CSWSH defense) and capture the cookie
/// `exp` so the socket is closed when the session expires. A request that fails
/// the cookie+Origin check is rejected with `401 no-store` BEFORE the upgrade.
pub async fn debug_ws(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // The auth layer attaches the shared `DashboardAuth` so the WS handler can
    // re-validate cookie+Origin (the cookie was already checked by the layer,
    // but the Origin allow-list is WS-specific and the `exp` drives the close).
    let auth = match gateway.dashboard_auth() {
        Some(auth) => auth,
        // No auth context (debug UI disabled / not registered) — should be
        // unreachable because the route only registers when auth exists, but
        // fail closed rather than serving an unauthenticated socket.
        None => {
            return crate::dashboard_auth::no_store(
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
            );
        }
    };
    let Some(exp) = auth.authenticate_ws(&headers) else {
        return crate::dashboard_auth::no_store(
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
        );
    };
    upgrade
        .on_upgrade(move |socket| debug_socket(socket, gateway, exp))
        .into_response()
}

async fn debug_socket(mut socket: WebSocket, gateway: Arc<Gateway>, session_exp: u64) {
    let mut receiver = gateway.subscribe_monitor();
    let snapshot = gateway.debug_snapshot();
    for message in snapshot.messages {
        if !send_message(&mut socket, &message).await {
            return;
        }
    }

    // Close the socket when the session cookie expires (no WS outliving its
    // cookie). `session_exp == u64::MAX` (dev-open: no token configured) yields
    // an effectively-infinite timer that never fires; a real cookie carries a
    // bounded future `exp`.
    let expiry = wait_for_session_expiry(session_exp);
    tokio::pin!(expiry);

    loop {
        tokio::select! {
            // Session expired mid-connection: close the socket.
            _ = &mut expiry => {
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            received = receiver.recv() => {
                match received {
                    Ok(update) if update.sequence <= snapshot.last_sequence => {}
                    Ok(update) => {
                        for message in update.messages {
                            if !send_message(&mut socket, &message).await {
                                return;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => return,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

/// Sleep until the session `exp` (unix secs) is reached, then return. A
/// non-positive remaining duration (already expired / clock skew) returns after
/// a zero-duration sleep (effectively immediate). The duration is derived from
/// the wall clock (`SystemTime`); the wait itself is a `tokio::time` sleep, so a
/// paused-clock test can drive it deterministically with `tokio::time::advance`.
async fn wait_for_session_expiry(session_exp: u64) {
    tokio::time::sleep(session_remaining(session_exp)).await;
}

/// A far-future cap for the expiry timer. The dev-open path (no token) passes
/// `session_exp == u64::MAX`; capping the computed remaining time keeps
/// `tokio::time::sleep` from overflowing the underlying `Instant` arithmetic on
/// an absurd duration (the socket has no real cookie expiry to honor there and
/// closes on disconnect/lag regardless). A bounded real cookie `exp` (≤ 1 h) is
/// always far below this cap.
const MAX_EXPIRY_WAIT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Remaining time until `session_exp` (unix secs), saturating at zero and capped
/// at [`MAX_EXPIRY_WAIT`].
fn session_remaining(session_exp: u64) -> Duration {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Duration::from_secs(session_exp.saturating_sub(now)).min(MAX_EXPIRY_WAIT)
}

async fn send_message(socket: &mut WebSocket, message: &DebugWsMessage) -> bool {
    let Ok(payload) = serde_json::to_string(message) else {
        return true;
    };
    socket.send(Message::Text(payload.into())).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    #[test]
    fn expired_session_has_zero_remaining() {
        assert_eq!(
            session_remaining(now_unix().saturating_sub(60)),
            Duration::ZERO
        );
        assert_eq!(session_remaining(0), Duration::ZERO);
    }

    #[test]
    fn future_session_has_positive_remaining() {
        let remaining = session_remaining(now_unix() + 120);
        assert!(
            remaining > Duration::from_secs(60),
            "remaining: {remaining:?}"
        );
        assert!(remaining <= Duration::from_secs(120));
    }

    /// The per-connection expiry timer fires once the cookie `exp` passes. We
    /// arm it for a 2s-future `exp`, then advance the paused clock past it and
    /// assert the wait completes — the same future the socket `select!`s on to
    /// send a `Close`.
    #[tokio::test(start_paused = true)]
    async fn expiry_wait_completes_after_exp_passes() {
        let exp = now_unix() + 2;
        let waiter = tokio::spawn(wait_for_session_expiry(exp));
        // Before the deadline the waiter is still pending.
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!waiter.is_finished(), "must not close before exp");
        // Past the deadline it resolves.
        tokio::time::advance(Duration::from_secs(2)).await;
        waiter.await.expect("expiry wait completes");
    }

    /// An already-expired cookie closes promptly (zero-duration wait).
    #[tokio::test(start_paused = true)]
    async fn expiry_wait_completes_immediately_when_already_expired() {
        let exp = now_unix().saturating_sub(10);
        // A zero-duration sleep still needs the timer to be polled once; yield
        // by advancing zero so the runtime drives it.
        let waiter = tokio::spawn(wait_for_session_expiry(exp));
        tokio::time::advance(Duration::from_millis(1)).await;
        waiter
            .await
            .expect("already-expired wait completes immediately");
    }
}
