# Codex Subscription Sidecar

`codex-subscription-proxy` is an experimental, local-only bridge for this path:

```text
Claude Code -> Anthropic Messages -> llmconduit -> OpenAI Responses
            -> codex-subscription-proxy -> ChatGPT Codex subscription backend
```

This is not a supported OpenAI Platform API or a substitute for an API key. It uses Codex's
private ChatGPT transport, pinned to Codex source revision
`9ff47868eb2afeec579183e01bb9d3d3e9df2bcd`. Revalidate the contract before changing that pin.

The sidecar source lives in the sibling project `../codex-subscription-proxy`. It binds only to a
loopback address, requires a separate local bearer token on every non-health route, accepts only
`gpt-5.6-sol`, and loads ChatGPT credentials through Codex's `AuthManager`. It never accepts a
ChatGPT access token over HTTP.

## Credential setup

Use a dedicated `CODEX_HOME`; do not share the home of a concurrently running Codex CLI or app.
The directory must be owned by the service user with mode `0700`, and its `auth.json` must be a
non-symlink regular file with mode `0600`. The sidecar holds an exclusive credential-owner lock for
its lifetime. Authenticate that home using the pinned/current Codex CLI, then start the sidecar
with:

```bash
CODEX_HOME=/absolute/path/to/dedicated/codex-home codex login --device-auth
export CODEX_SUBSCRIPTION_PROXY_CODEX_HOME=/absolute/path/to/dedicated/codex-home
export CODEX_SUBSCRIPTION_PROXY_TOKEN='<dedicated random token of at least 32 bytes>'
cargo run --release --manifest-path ../codex-subscription-proxy/Cargo.toml
```

Optional resource limits are documented in the sidecar README. `/health` is public and
account-opaque; `/ready`, `/v1/models`, and `/v1/responses` require the local bearer token.

## llmconduit routing entry

Add the sidecar as a routing provider. Keep any existing Chat Completions provider unchanged:

```yaml
upstreams:
  - name: local-vllm
    upstream_base_url: http://127.0.0.1:8000/v1

  - name: codex-subscription
    upstream_base_url: http://127.0.0.1:5033/v1
    upstream_api_key: <same dedicated local sidecar token>
    wire_api: codex_responses
    responses_capabilities:
      parallel_tool_calls: false
      structured_outputs: [text, json_schema]
      reasoning_summary: upstream
      encrypted_reasoning: passthrough
      input_image: reject
```

`wire_api: codex_responses` is the critical switch. It bypasses the lossy Responses-to-Chat lowering and
preserves native output items, call IDs, argument deltas, reasoning summaries, encrypted reasoning,
and provider usage. Every member of one nested failover chain must use the same wire protocol; a
mixed chain is rejected at configuration load.

The sidecar forces the current Responses-Lite rules for Sol: `stream:true`, `store:false`,
`tool_choice:auto`, `parallel_tool_calls:false`, provider-owned prompt-cache affinity, and encrypted
reasoning inclusion. llmconduit's public `store` and `previous_response_id` semantics remain local
and independent.

## Safe smoke test

Use temporary ports and configuration rather than changing the production service. Verify, in
order:

1. Authenticated sidecar readiness and model discovery.
2. A streaming text request directly to the sidecar.
3. A text request through llmconduit's `/v1/responses` endpoint.
4. An Anthropic `/v1/messages` function call followed by its `tool_result` continuation.
5. A Claude Code run against the temporary llmconduit endpoint.

Never print the local bearer, ChatGPT tokens, account ID, private turn-state header, or request
bodies during validation. The automated suites use fake backends and sentinel-secret checks; live
subscription tests remain opt-in.

## Anthropic token counting

The private Codex subscription surface has no exact input-token counting endpoint. For a native
Responses provider, llmconduit's `/v1/messages/count_tokens` therefore returns a deterministic local
estimate without starting a billed model generation: `ceil(serialized native request bytes / 3) + 64`.
The response keeps Anthropic's standard `{ "input_tokens": N }` body and adds
`x-llmconduit-token-count-quality: estimated`. This estimate is deliberately conservative relative
to llmconduit's ordinary four-byte streaming hint, but it is not a tokenizer result or a guaranteed
upper bound. Chat Completions providers with a working `/tokenize` endpoint continue to return the
provider count and are marked `x-llmconduit-token-count-quality: exact`.

## Current limits

- Single-user, loopback-only, and experimental.
- GPT-5.6 Sol only.
- Claude Code token counting is an explicitly tagged estimate,
  `ceil(serialized native request bytes / 3) + 64`. It is approximate rather than a tokenizer
  upper bound, and it performs no model generation.
- Anthropic's mandatory `max_tokens` is validated but cannot be enforced by the pinned private
  Responses-Lite request, which has no output-token-limit field. The sidecar removes the translated
  compatibility hint before dispatch, so the subscription backend controls the actual ceiling.
- Text input and ordinary client function tools only; hosted tools, files, and image inputs are
  rejected.
- Sol's current Responses-Lite path disables parallel tool calls.
- Subscription policy, entitlement, quota, and the private backend contract can change without
  notice.
