//! Native Anthropic transport, separate from the canonical Responses engine.
//!
//! The destination is fixed by validated configuration. Incoming OAuth credentials
//! never participate in provider selection, redirects, retries, or fallback.

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Uri, header};
use axum::response::Response;
use futures::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use url::Url;

use crate::config::{Config, glob_to_regex};
use crate::error::{AppError, AppResult};

/// The Anthropic first-party model families that use the subscription, as they
/// appear in a request `model` field. Claude Code resolves a short alias such as
/// `opus` to a full id such as `claude-opus-5-5` before sending it, so these
/// patterns match both forms. Every other model string reaches the local routes.
pub(crate) const ANTHROPIC_MODEL_FAMILIES: [&str; 3] =
    ["claude-fable-*", "claude-opus-*", "claude-haiku-*"];

/// Opt-in configuration. No upstream credential is stored here: Claude Code
/// supplies and refreshes its subscription bearer token on each request.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersistedAnthropicPassthrough {
    pub upstream_origin: String,
    /// Optional explicit selectors. When omitted,
    /// [`ANTHROPIC_MODEL_FAMILIES`] is the selector: a fable, opus, or haiku
    /// model uses the subscription, and every other model string reaches the
    /// local routes.
    #[serde(default)]
    pub rules: Vec<PersistedPassthroughRule>,
}

/// Conditions within a rule are ANDed; the first matching rule selects native
/// passthrough. Header values are exact and case-sensitive.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersistedPassthroughRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct AnthropicPassthrough {
    origin: Url,
    rules: Vec<PassthroughRule>,
}

#[derive(Clone)]
struct PassthroughRule {
    model: Option<Regex>,
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl AnthropicPassthrough {
    pub fn resolve(config: &PersistedAnthropicPassthrough) -> Result<Self, String> {
        // Do not include the supplied URL in errors: a typo may embed a token.
        let origin = Url::parse(&config.upstream_origin).map_err(
            |_| "anthropic_passthrough.upstream_origin must be https://api.anthropic.com",
        )?;
        if origin.as_str() != "https://api.anthropic.com/" {
            return Err(
                "anthropic_passthrough.upstream_origin must be https://api.anthropic.com (no credentials, custom port, path, query, or fragment)".into(),
            );
        }
        // An omitted `rules` list selects the Anthropic first-party model
        // families, so a fable, opus, or haiku model uses the subscription
        // while every other model string reaches the local routes. An explicit
        // list replaces the default entirely.
        let default_rules;
        let configured_rules = if config.rules.is_empty() {
            default_rules = ANTHROPIC_MODEL_FAMILIES
                .iter()
                .map(|pattern| PersistedPassthroughRule {
                    model: Some((*pattern).to_string()),
                    headers: BTreeMap::new(),
                })
                .collect();
            &default_rules
        } else {
            &config.rules
        };
        let rules = configured_rules
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                let invalid = |reason| format!("anthropic_passthrough.rules[{index}]: {reason}");
                if rule.model.is_none() && rule.headers.is_empty() {
                    return Err(invalid(
                        "at least one model or header condition is required",
                    ));
                }
                let model = rule
                    .model
                    .as_deref()
                    .map(|pattern| {
                        if pattern.trim().is_empty() || pattern.trim() != pattern {
                            return Err(invalid("model must be nonempty without outer whitespace"));
                        }
                        glob_to_regex(pattern).map_err(|_| invalid("invalid model glob"))
                    })
                    .transpose()?;
                let mut seen = HashSet::new();
                let headers = rule
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        let name = HeaderName::from_bytes(name.as_bytes())
                            .map_err(|_| invalid("invalid header name"))?;
                        if !(name.as_str().starts_with("x-claude-code-")
                            || name == "x-llmconduit-route")
                        {
                            return Err(invalid(
                                "header selectors must use x-claude-code-* or x-llmconduit-route",
                            ));
                        }
                        if !seen.insert(name.clone()) {
                            return Err(invalid("duplicate case-insensitive header name"));
                        }
                        let value = HeaderValue::from_str(value)
                            .map_err(|_| invalid("invalid header value"))?;
                        if value.is_empty() {
                            return Err(invalid("header value must not be empty"));
                        }
                        Ok((name, value))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok(PassthroughRule { model, headers })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self { origin, rules })
    }

    pub(crate) fn matching_rule(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Option<usize> {
        if method != Method::POST
            || !matches!(uri.path(), "/v1/messages" | "/v1/messages/count_tokens")
        {
            return None;
        }
        // Deserialize only the discriminator. Unknown fields are skipped and the
        // original bytes remain the transport payload, including JSON whitespace.
        // Duplicate model fields fail parsing instead of selecting ambiguously.
        #[derive(Deserialize)]
        struct ModelSelector {
            model: Option<String>,
        }
        let model = if self.rules.iter().any(|rule| rule.model.is_some()) {
            serde_json::from_slice::<ModelSelector>(body)
                .ok()
                .and_then(|selector| selector.model)
        } else {
            None
        };
        self.rules.iter().position(|rule| {
            rule.model.as_ref().is_none_or(|pattern| {
                model
                    .as_deref()
                    .is_some_and(|model| pattern.is_match(model))
            }) && rule.headers.iter().all(|(name, expected)| {
                let mut values = headers.get_all(name).iter();
                values.next() == Some(expected) && values.next().is_none()
            })
        })
    }
}

pub(crate) struct AnthropicProxy {
    config: AnthropicPassthrough,
    client: reqwest::Client,
}

impl AnthropicProxy {
    pub(crate) fn new(config: AnthropicPassthrough, gateway: &Config) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(gateway.connect_timeout())
            .timeout(gateway.request_timeout)
            .tcp_nodelay(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .expect("Anthropic passthrough HTTP client");
        Self { config, client }
    }

    pub(crate) fn matching_rule(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Option<usize> {
        self.config.matching_rule(method, uri, headers, body)
    }

    pub(crate) async fn forward(
        &self,
        uri: &Uri,
        headers: &HeaderMap,
        body: Bytes,
    ) -> AppResult<Response> {
        let mut outgoing = request_headers(headers);
        let mut authorization = outgoing.get_all(header::AUTHORIZATION).iter();
        let bearer = authorization
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .is_some_and(|(scheme, token)| {
                scheme.eq_ignore_ascii_case("bearer")
                    && !token.is_empty()
                    && !token.bytes().any(|byte| byte.is_ascii_whitespace())
            });
        if !bearer || authorization.next().is_some() {
            return Err(AppError::unauthorized(
                "Anthropic passthrough requires subscription OAuth bearer authorization",
            ));
        }
        if let Some(value) = outgoing.get_mut(header::AUTHORIZATION) {
            value.set_sensitive(true);
        }
        let mut destination = self.config.origin.clone();
        destination.set_path(uri.path());
        destination.set_query(uri.query());
        let upstream = self
            .client
            .post(destination)
            .headers(outgoing)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                // reqwest's Display can include the URL/query. Only bounded
                // transport classifications belong in diagnostics for this path.
                tracing::warn!(
                    timeout = error.is_timeout(),
                    connect = error.is_connect(),
                    "Anthropic passthrough transport failed"
                );
                AppError::upstream("Anthropic passthrough transport failed")
            })?;
        let status = upstream.status();
        let headers = response_headers(upstream.headers());
        // The response owns the upstream stream. Dropping the downstream body
        // drops the upstream request; no detached reader or retry can continue it.
        let stream = upstream.bytes_stream().map(|chunk| {
            chunk.map_err(|error| {
                tracing::warn!(
                    timeout = error.is_timeout(),
                    "Anthropic passthrough stream failed"
                );
                std::io::Error::other("Anthropic passthrough stream failed")
            })
        });
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        Ok(response)
    }
}

fn hop_headers(headers: &HeaderMap) -> HashSet<HeaderName> {
    let mut names = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "proxy-connection",
    ]
    .into_iter()
    .map(HeaderName::from_static)
    .collect::<HashSet<_>>();
    for value in headers.get_all(header::CONNECTION) {
        if let Ok(value) = value.to_str() {
            for name in value.split(',') {
                if let Ok(name) = HeaderName::from_bytes(name.trim().as_bytes()) {
                    names.insert(name);
                }
            }
        }
    }
    names
}

fn request_headers(headers: &HeaderMap) -> HeaderMap {
    let hop = hop_headers(headers);
    let mut outgoing = HeaderMap::new();
    for (name, value) in headers {
        // Open-ended vendor namespaces preserve beta and SDK extensions. Other
        // credentials, cookies, forwarding headers, Host, and gateway routing
        // hints stay local. reqwest computes Host and Content-Length itself.
        let allowed = name.as_str().starts_with("anthropic-")
            || name.as_str().starts_with("x-claude-code-")
            || name.as_str().starts_with("x-stainless-")
            || matches!(
                name.as_str(),
                "authorization"
                    | "accept"
                    | "accept-encoding"
                    | "content-type"
                    | "content-encoding"
                    | "user-agent"
                    | "x-app"
                    | "x-request-id"
            );
        if allowed && !hop.contains(name) {
            outgoing.append(name.clone(), value.clone());
        }
    }
    outgoing
}

fn response_headers(headers: &HeaderMap) -> HeaderMap {
    let hop = hop_headers(headers);
    let mut outgoing = HeaderMap::new();
    for (name, value) in headers {
        if !hop.contains(name) {
            outgoing.append(name.clone(), value.clone());
        }
    }
    outgoing
}

#[cfg(test)]
mod tests;
