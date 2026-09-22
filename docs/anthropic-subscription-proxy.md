# Native Anthropic subscription proxy

Status: **implemented** and **qualified with local mock upstreams**. Live Claude
Code subscription authentication and mixed-provider agent teams require the
verification procedure below. Mock tests do not establish account entitlement or
local-model tool compatibility.

`anthropic_passthrough` lets one Claude Code session use native Anthropic
Messages for selected models and llmconduit's translation path for other models.
The gateway chooses a transport per request; Claude Code still owns its agents,
team messaging, login, and token refresh.

## Selection and configuration

The [example configuration](anthropic-subscription.example.yaml) maps:

| Request model | Transport | Destination |
|---|---|---|
| `claude-fable-*` | Native Anthropic Messages | `https://api.anthropic.com` |
| `claude-opus-*` | Translated Chat Completions | `http://127.0.0.1:8000/v1` |
| `claude-haiku-*` | Translated Chat Completions | `http://127.0.0.1:8001/v1` |

Both translated routes send `deepseek-ai/DeepSeek-V4.1-Flash` as the backend model
ID. Ordinary `model_routes` retain their catalog-first precedence and
unmatched-model behavior. This assigns requests explicitly to two replicas; it
does not load balance. Backend profiles must use the full served model ID.

Native selection reads the top-level JSON `model` before adapters, catalog
lookup, defaults, profiles, and prompt injection. Patterns use the same
case-insensitive glob syntax as `model_routes`: `*`, `?`, and character classes.
Exact IDs also work. Model rules require valid JSON with a single string `model`;
missing, duplicate, or malformed model fields do not match. The inspected body
is never reserialized.

Header conditions use exact, case-sensitive values. All conditions within one
rule must match; the first matching rule wins. A header-only rule can select an
opaque body. At least one condition and one rule are required. Unknown
configuration fields are rejected. Header names are case-insensitive and limited
to `x-claude-code-*` and the custom header `x-llmconduit-route`.

```yaml
anthropic_passthrough:
  upstream_origin: "https://api.anthropic.com"
  rules:
    - model: "claude-fable-*"
      headers:
        x-claude-code-request-class: "main"
    - headers:
        x-llmconduit-route: "anthropic"
```

Prefer the model-only example when Fable workflow, compaction, and token-count
requests must follow the same route. Requiring class `main` excludes requests
with another or absent class. Client-supplied selectors are routing hints, not
an authentication boundary.

Claude Code 2.1.274 and captured Messages bodies were inspected: requests contain
full IDs such as `claude-opus-4-8`, not only UI aliases such as `opus`. No
Fable-specific header is assumed. Claude Code documents optional request-class
and agent-type hints; custom-base-URL clients enable them with
`CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`. Verify the installed client's behavior
before requiring headers. See the official
[gateway compatibility guide](https://code.claude.com/docs/en/llm-gateway-protocol).

## Subscription authentication

Set `ANTHROPIC_BASE_URL` to the gateway while retaining the saved subscription
login. Do not add `ANTHROPIC_API_KEY`, a gateway auth-token override, or
`apiKeyHelper`: gateway credentials replace the subscription credential.
Claude Code owns OAuth renewal; llmconduit neither stores nor refreshes tokens.
See [subscriptions and gateways](https://code.claude.com/docs/en/llm-gateway#subscriptions-and-gateways).

Selected requests require one nonempty `Authorization: Bearer ...` header.
llmconduit validates its shape; Anthropic validates the opaque credential and
account entitlement. `x-api-key` is never an Anthropic authentication fallback.
Native 401, 403, and usage-limit responses return unchanged. There is no retry,
provider failover, compatibility-endpoint conversion, or key billing fallback.

`upstream_origin` must resolve to exactly `https://api.anthropic.com/`, with no
credentials, custom port, path, query, or fragment. Requests cannot choose another
origin. The client disables redirects, environment HTTP proxies, and automatic
decompression, and uses normal TLS certificate validation. Bearer tokens are
marked sensitive in the HTTP client and never handed to translated upstreams.

The example binds loopback without `LLMCONDUIT_API_TOKEN`. Existing gateway API
authentication still runs before native routing: that token requires a separate
gateway credential and does not accept Anthropic OAuth as gateway authentication.
Do not expose the unauthenticated example listener publicly. Startup's
non-loopback authentication requirements remain in force.

## Wire behavior and limits

- Only `POST /v1/messages` and `POST /v1/messages/count_tokens` participate. Both
  use identical rules and preserve path, query, and body. Model-based selection
  expects uncompressed JSON; explicit header routing can select encoded bodies.
- Inbound bodies retain the `max_request_body_bytes` limit and are buffered for
  inspection. Responses stream without complete-body buffering or SSE parsing.
  Pings, comments, unknown events, usage fields, errors, and encoded bytes remain
  intact. Network chunk boundaries may differ.
- Request headers in the `anthropic-*`, `x-claude-code-*`, and `x-stainless-*`
  namespaces pass through, along with authorization, accept/encoding, content
  type/encoding, user agent, `x-app`, and `x-request-id`. Full `anthropic-beta`
  values, including OAuth capability, are preserved. Other headers, cookies, API
  keys, and gateway routing hints stay local. Host and request Content-Length are
  generated for the upstream request.
- Response status and end-to-end headers pass through, including request IDs,
  retry/usage-limit headers, content encoding, and redirect Location. Standard
  hop-by-hop headers and headers named by Connection are removed in both
  directions. HTTP trailers and WebSocket upgrades are unsupported.
- `connect_timeout_secs` bounds connecting; `request_timeout_secs` bounds the
  entire response, including streams. Connection failures produce sanitized local
  502 responses. Transport failures after headers terminate the stream without
  translated events. Disconnects cancel the upstream request, including while
  awaiting response headers.
- Native traffic logs only generated request ID, endpoint, rule index, status,
  and time to headers. Bodies, incoming header values, and queries are not
  captured even with payload logging enabled. Native requests bypass turn capture,
  dashboard flow records, usage accounting, replay, and capture-required response
  gates. Those features still apply to translated requests. Subscription usage
  remains available in Anthropic responses and the account UI.
- `/v1/models` still describes translated providers. Passthrough rules neither
  discover Anthropic models nor prove model access. Select the lead's full model
  ID explicitly. `/api/hello` remains a 404 probe.
- Local models depend on the Anthropic adapters and their own tool-use ability.
  Routing alone does not qualify native team behavior.

Omitting `anthropic_passthrough` disables it. Interactive `configure` preserves
the block without prompting for its fields.

## Verification

Local tests use fake bearer tokens and mock Anthropic/vLLM servers:

```bash
cargo test --lib anthropic_proxy
cargo test --all-targets
cargo clippy --all-targets
cargo fmt --all --check
```

The focused suite covers matching, trusted destinations, byte/header
preservation, count-token/error responses, redirects, credential isolation,
existing translation, body limits, diagnostics, incremental SSE delivery, and
disconnects over real loopback TCP connections.

For live verification, first authorize a staging run or deployment under the
operational runbook. Evaluate the example on port 5023 alongside the existing
gateway without changing saved Claude settings:

1. Verify both vLLM `/v1/models` endpoints advertise
   `deepseek-ai/DeepSeek-V4.1-Flash`. Build llmconduit and run
   `target/debug/llmconduit start --config docs/anthropic-subscription.example.yaml`
   without API credential overrides. Check `/health`.
2. Start Claude Code with the saved subscription login and a process-scoped URL:
   `ANTHROPIC_BASE_URL=http://127.0.0.1:5023 claude --model 'claude-fable-5-1[1m]'`.
   Substitute a full entitled Fable ID if needed. Check `/status` for subscription
   authentication. Keep OAuth tokens out of curl, files, and shell history.
3. Ask for a short lead response and an ordinary tool call. Check for the
   `Anthropic passthrough response prepared` event and native response. A 401 must
   remain an authentication failure; resolve login through Claude Code.
4. Ask the lead to create native teammates using `opus` and `haiku`. Confirm
   translated requests reach ports 8000 and 8001 with the full DeepSeek ID. Have
   teammates exchange a message and complete a shared task. Do not invoke
   `lci-run` for this check.
5. Trigger token counting and confirm matching Fable count requests use the native
   endpoint when emitted by the client. Interrupt a streaming lead response and
   verify upstream cancellation. Inspect sanitized diagnostics: subscription
   headers and native payloads must be absent from llmconduit captures.

Live verification is complete when the lead, both local teammates, team messaging,
and cancellation work in the same Claude Code session.
