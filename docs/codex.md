# Codex CLI with llmconduit

This setup was validated against Codex CLI 0.144.4 and its `ModelInfo` catalog schema at
OpenAI Codex commit `8c68d4c87dc54d38861f5114e920c3de2efa5876`. It uses the Responses API at
`http://127.0.0.1:5022/v1/responses` and the local `GLM-5.2-NVFP4` model.

## Configure a dedicated provider

Use a provider-specific environment variable even when llmconduit is listening only on loopback:

```bash
export LLMCONDUIT_API_TOKEN='replace-with-a-dedicated-gateway-token'
```

This is llmconduit's gateway token, not an OpenAI credential. llmconduit accepts it through either
Bearer authorization or `x-api-key`. Loopback serving may be left unauthenticated, but using a
dedicated provider token prevents Codex from forwarding a global OpenAI credential by mistake.
Never point `env_key` at `OPENAI_API_KEY` or reuse a ChatGPT/OpenAI token for this local provider.

Codex 0.144.4 may reuse its stored global OpenAI/ChatGPT bearer when a custom provider has neither
`env_key` nor another provider-scoped bearer, even with `requires_openai_auth = false`. For an
intentionally unauthenticated local profile, use a clearly non-secret sentinel instead:

```toml
experimental_bearer_token = "llmconduit-local-unauthenticated"
```

This only selects provider-scoped request auth in Codex; it does not enable authentication in
llmconduit. Prefer `env_key = "LLMCONDUIT_API_TOKEN"` whenever the gateway has a real token, and do
not configure both fields.

## Declare the matching llmconduit capabilities

The Codex catalog is client-side metadata; it does not enable gateway features. Merge the following
block into the existing `GLM-5.2-NVFP4` model profile in llmconduit's configuration before using
the profile below:

```yaml
model_profiles:
  GLM-5.2-NVFP4:
    responses_capabilities:
      parallel_tool_calls: false
      structured_outputs: [text, json_object, json_schema]
      reasoning_summary: upstream
      encrypted_reasoning: passthrough
      agent_message_encrypted_content: plaintext_compat
      input_image: placeholder
      input_file: unsupported
      truncation_auto: unsupported
      text_verbosity: unsupported
      service_tiers: []
      prompt_cache_key: gateway_hash
      prompt_cache_retention: []
```

This declaration matches the checked catalog: Codex sends sequential text/tool turns, requests an
explicit upstream reasoning summary, includes `reasoning.encrypted_content` whenever reasoning is
enabled, and attaches an opaque prompt-cache key that llmconduit hashes locally instead of
forwarding. `encrypted_reasoning: passthrough` authorizes only opaque state actually supplied by the
provider; llmconduit does not synthesize it from reasoning text.
`agent_message_encrypted_content: plaintext_compat` is a separate Codex-v2 compatibility policy:
Codex places the local model's plain-text collaboration payload in an `encrypted_content` part, and
llmconduit makes that part visible to the selected Chat backend while retaining its canonical type
in Responses state. Do not enable this policy for a backend that supplies genuinely opaque
ciphertext. Images remain absent from the Codex
catalog even though the gateway's non-native-image fallback is `placeholder`. Do not declare a
service tier, verbosity, file input, or automatic truncation until that behavior is supported and
tested by the selected upstream. If the upstream does not provide an explicit safe reasoning
summary channel, disable reasoning in Codex and use catalog metadata that does not advertise
reasoning instead of deriving a summary from hidden reasoning.

The snippet is documentation only: it does not edit the live configuration or restart the service.
Applying it to a running installation is a separate operational change.

Codex 0.134.0 and later load named profiles from separate files. Put the following in
`~/.codex/responses.config.toml`:

```toml
model = "GLM-5.2-NVFP4"
model_provider = "llmconduit"
model_catalog_json = "/home/jon/git/local-inference-lab/llmconduit/docs/examples/codex-model-catalog.glm-5.2-nvfp4.json"
personality = "none"
web_search = "disabled"

# These match the active llmconduit model profile.
model_reasoning_effort = "max"
model_reasoning_summary = "auto"

[features]
# Keep the verified local-agent tools enabled even if Codex defaults change.
apps = true
goals = true
multi_agent = true
plugins = true
shell_tool = true
tool_suggest = true
unified_exec = true

[tools.experimental_request_user_input]
enabled = true

[model_providers.llmconduit]
name = "Local llmconduit"
base_url = "http://127.0.0.1:5022/v1"
env_key = "LLMCONDUIT_API_TOKEN"
wire_api = "responses"
requires_openai_auth = false
```

`model_catalog_json` must be an absolute path and is loaded only at Codex startup. The provider
`base_url` ends at `/v1`; Codex appends `/responses` itself. For installations where global Codex
credentials still interfere, use an isolated home while testing:

```bash
CODEX_HOME="$(mktemp -d)" codex exec --ephemeral \
  -c 'model="GLM-5.2-NVFP4"' \
  -c 'model_provider="llmconduit"' \
  -c 'model_catalog_json="/home/jon/git/local-inference-lab/llmconduit/docs/examples/codex-model-catalog.glm-5.2-nvfp4.json"' \
  -c 'personality="none"' \
  -c 'web_search="disabled"' \
  -c 'model_providers.llmconduit.name="Local llmconduit"' \
  -c 'model_providers.llmconduit.base_url="http://127.0.0.1:5022/v1"' \
  -c 'model_providers.llmconduit.env_key="LLMCONDUIT_API_TOKEN"' \
  -c 'model_providers.llmconduit.wire_api="responses"' \
  'Reply with exactly: ok'
```

Keep `web_search = "disabled"` explicit for this text-only catalog. Codex 0.144.4 otherwise sends
a disabled hosted-search declaration containing `external_web_access: false`; llmconduit correctly
rejects that unsupported hosted-search control instead of silently discarding it.

Start Codex with the profile:

```bash
codex --profile responses
codex exec --profile responses --ephemeral "Reply with exactly: ok"
```

## Why the local catalog is required

llmconduit's `GET /v1/models` intentionally returns the standard OpenAI list shape
(`{"object":"list","data":[...]}`). Codex 0.144.4 also has a private model-discovery shape with a
top-level `models` array and rich per-model metadata. Pointing Codex directly at the standard route
therefore produces a catalog warning and makes Codex fall back to generic metadata.

The checked-in catalog supplies Codex's private metadata without changing llmconduit's public
OpenAI-compatible route. It advertises:

- a 524,288-token context window with a 95% effective-input percentage;
- text input only;
- sequential model-emitted tool calls (`supports_parallel_tool_calls = false`);
- freeform `apply_patch` and the v2 collaboration tool family;
- client-side dynamic-tool search metadata (the catalog's `supports_search_tool` gate exposes
  `tool_search` even though the old feature flag was removed), but no hosted web search;
- no verbosity control, image-original mode, or service tiers;
- reasoning summaries and the reasoning efforts mapped by the active llmconduit model profile; and
- the `shell_command` tool shape used by Codex.

The catalog deliberately embeds Codex 0.144.4's fallback base instructions. Those instructions and
Codex's tool schemas account for substantial input tokens even on a trivial prompt; retaining them
preserves agent behavior. Replacing them with a short prompt may reduce token accounting, but it is
a behavioral change rather than a compatibility fix.

Codex v2 collaboration depends on two Responses extensions in addition to advertising the tools:
llmconduit preserves `namespace` on returned function calls so Codex dispatches them through the
`collaboration` runtime, and it accepts Codex `agent_message` continuations. Because the configured
upstream speaks Chat Completions, `agent_message` is normalized to a user-turn boundary while its
ordered `input_text` and `encrypted_content` payload remains model-visible. Request logging stays
metadata-only by default, so that collaboration payload is not added to normal logs.

The profile explicitly enables `request_user_input`; Codex makes it callable in Plan mode. That
tool and tools discovered through `tool_search` work in the interactive TUI, but Codex 0.144.4
deliberately rejects their frontend interactions in `codex exec`; use the TUI to exercise those
continuations.
`view_image` remains visible as a generic workspace utility, but Codex rejects it before dispatch
for this intentionally text-only catalog. Do not advertise image input merely to silence that
guard.

## Upgrade checklist

Treat the catalog as Codex-version-specific. After upgrading Codex:

1. Compare the new `codex_protocol::openai_models::ModelInfo` fields and enum values with the JSON.
2. Refresh `base_instructions` from the new version's fallback model instructions if they changed.
3. Reconfirm the model's context size, input modalities, reasoning mappings, tool parallelism, and
   service-tier support against the active llmconduit configuration.
4. Parse the catalog with the new CLI before regular use. Run a bounded
   `codex exec --profile responses --strict-config --ephemeral "Reply with exactly: ok"` smoke;
   Codex 0.144.4 does not apply profiles to the `features list` subcommand.

Do not replace llmconduit's standard `/v1/models` response with Codex's private catalog schema; use
the static catalog for Codex and retain the standard route for OpenAI-compatible clients.

## Opt-in smoke test

`scripts/codex-responses-smoke.sh` exercises both Codex and the public Responses state surface
against an explicitly supplied alternate llmconduit endpoint. It refuses the normal live port
`:5022`, uses an isolated `CODEX_HOME` and read-only temporary workspace, and never starts, stops,
reconfigures, or restarts llmconduit.

The fixed battery covers:

- exact plain-text output with no tools;
- one safe shell call followed by tool-output continuation;
- two distinct sequential shell calls, with their order checked;
- a two-file, multi-step read-only task;
- a direct stored Response followed by `previous_response_id` continuation;
- a parameter-specific OpenAI 400 for an unsupported `service_tier`; and
- Codex warning detection plus served usage, cached-token, and reasoning-token recording.

The harness uses `umask 077`, removes write permission from the workspace while Codex runs, unsets
global OpenAI/ChatGPT credentials for every Codex child, and supplies the dedicated gateway token
only through the configured provider. It scans retained artifacts for that token, any inherited
OpenAI/ChatGPT credential, and an opaque prompt-cache sentinel. If one appears, all artifacts are
deleted. The direct parent response first asserts the standard `prompt_cache_key` echo and then
redacts that one expected client-visible field in its retained artifact; any appearance elsewhere
still fails the scan. Successful artifacts contain controlled JSONL, direct Responses resources,
stderr, final messages, `usage-summary.json`, and `external-observation.json` under a private
`/tmp` directory.

Start a separately authorized test instance on another loopback port, then run:

```bash
LLMCONDUIT_CODEX_SMOKE_BASE_URL=http://127.0.0.1:15022/v1 \
LLMCONDUIT_API_TOKEN='test-instance-token-at-least-16-bytes' \
bash scripts/codex-responses-smoke.sh
```

The harness never starts, stops, reconfigures, or restarts llmconduit. It intentionally requires an
already-running alternate instance and does not inspect the production service.

### Optional raw-upstream observation

Codex's client-visible JSONL cannot prove that raw upstream token counts and function-call
identities survived conversion unchanged, nor can it see the headers llmconduit sent upstream.
Those checks require a separately authorized test-only observer at the mock/reverse upstream or a
sanitized turn-capture seam. Point `LLMCONDUIT_CODEX_SMOKE_OBSERVATION_JSON` at its output to make
the smoke harness enforce the comparison instead of recording the limitation as `not_run`:

```json
{
  "schema_version": 1,
  "cases": [
    {
      "name": "one-command",
      "raw_usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120},
      "served_usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120},
      "raw_call_identities": ["call_01"],
      "served_call_identities": ["call_01"]
    }
  ],
  "upstream_request_headers": [
    {"name": "authorization", "value_sha256": "0000000000000000000000000000000000000000000000000000000000000000"}
  ]
}
```

For every case, the script requires exact equality between `raw_usage` and `served_usage` and
between the ordered raw/served call-identity arrays. Header values must be represented only by a
lowercase SHA-256 digest; the script rejects the digest of both the raw gateway token and its
`Bearer ` form. An upstream's own configured credential may legitimately produce a different
`authorization` digest. The observer must include every captured upstream request header as its
lowercase 64-character value digest; the smoke script requires at least one header observation.
The hook file must already be sanitized and must not contain prompt or response bodies. Without
this hook, the normal smoke cases still validate Codex parsing and tool continuations, but
raw-upstream equality and header isolation remain explicitly unobserved.

To retain artifacts under a specific private parent directory, set
`LLMCONDUIT_CODEX_SMOKE_ARTIFACT_DIR`. To use a catalog, model, Codex binary, or timeout other than
the defaults, set `LLMCONDUIT_CODEX_SMOKE_CATALOG`, `LLMCONDUIT_CODEX_SMOKE_MODEL`, `CODEX_BIN`, or
`LLMCONDUIT_CODEX_SMOKE_TIMEOUT_SECONDS`, respectively. Remote endpoints remain disabled unless
`LLMCONDUIT_CODEX_SMOKE_ALLOW_REMOTE=1` is explicitly set; an alternate loopback port is preferred.
