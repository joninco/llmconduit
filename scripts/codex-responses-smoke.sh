#!/usr/bin/env bash
set -euo pipefail

umask 077

die() {
  echo "codex smoke: $*" >&2
  exit 1
}

for command in curl jq sha256sum timeout; do
  command -v "$command" >/dev/null 2>&1 || die "required command not found: $command"
done

CODEX_BIN="${CODEX_BIN:-codex}"
command -v "$CODEX_BIN" >/dev/null 2>&1 || die "Codex CLI not found: $CODEX_BIN"

BASE_URL="${LLMCONDUIT_CODEX_SMOKE_BASE_URL:-}"
[[ -n "$BASE_URL" ]] || die "set LLMCONDUIT_CODEX_SMOKE_BASE_URL to an alternate /v1 endpoint"
[[ "$BASE_URL" == */v1 ]] || die "LLMCONDUIT_CODEX_SMOKE_BASE_URL must end in /v1"
case "$BASE_URL" in
  *://*:5022/v1)
    die "refusing to contact the normal live llmconduit port 5022"
    ;;
  http://127.0.0.1:*|http://localhost:*)
    ;;
  *)
    [[ "${LLMCONDUIT_CODEX_SMOKE_ALLOW_REMOTE:-}" == "1" ]] || \
      die "non-loopback endpoint requires LLMCONDUIT_CODEX_SMOKE_ALLOW_REMOTE=1"
    ;;
esac

[[ -n "${LLMCONDUIT_API_TOKEN:-}" ]] || die "set a dedicated test-instance LLMCONDUIT_API_TOKEN"
[[ ${#LLMCONDUIT_API_TOKEN} -ge 16 ]] || die "test-instance token must contain at least 16 bytes"

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CATALOG="${LLMCONDUIT_CODEX_SMOKE_CATALOG:-$REPO_DIR/docs/examples/codex-model-catalog.glm-5.2-nvfp4.json}"
[[ -f "$CATALOG" ]] || die "model catalog not found: $CATALOG"

MODEL="${LLMCONDUIT_CODEX_SMOKE_MODEL:-GLM-5.2-NVFP4}"
TIMEOUT_SECONDS="${LLMCONDUIT_CODEX_SMOKE_TIMEOUT_SECONDS:-240}"
[[ "$TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] || die "timeout must be a positive integer"

if [[ -n "${LLMCONDUIT_CODEX_SMOKE_ARTIFACT_DIR:-}" ]]; then
  mkdir -p "$LLMCONDUIT_CODEX_SMOKE_ARTIFACT_DIR"
  ARTIFACT_DIR="$(mktemp -d "$LLMCONDUIT_CODEX_SMOKE_ARTIFACT_DIR/llmconduit-codex-smoke.XXXXXX")"
else
  ARTIFACT_DIR="$(mktemp -d /tmp/llmconduit-codex-smoke.XXXXXX)"
fi
CODEX_HOME_DIR="$ARTIFACT_DIR/codex-home"
WORKSPACE="$ARTIFACT_DIR/workspace"
mkdir -p "$CODEX_HOME_DIR" "$WORKSPACE"
printf 'alpha\n' >"$WORKSPACE/alpha.txt"
printf 'beta\n' >"$WORKSPACE/beta.txt"
chmod -R a-w "$WORKSPACE"

AUTH_HEADER_FILE="$ARTIFACT_DIR/.gateway-auth-header"
printf 'Authorization: Bearer %s\n' "$LLMCONDUIT_API_TOKEN" >"$AUTH_HEADER_FILE"
chmod 600 "$AUTH_HEADER_FILE"
PROMPT_CACHE_SENTINEL="llmconduit-smoke-cache-key-7f8d2a1c"
STATE_MARKER="LLMCONDUIT_STATE_7F8D2A1C"

restore_workspace_permissions() {
  if [[ -d "$WORKSPACE" ]]; then
    chmod -R u+w "$WORKSPACE" 2>/dev/null || true
  fi
}

cleanup_on_exit() {
  restore_workspace_permissions
  rm -f "$AUTH_HEADER_FILE"
  if [[ -d "$ARTIFACT_DIR" ]] && artifact_contains_secret; then
    rm -rf "$ARTIFACT_DIR"
    echo "codex smoke: sensitive test data appeared in an artifact; all smoke artifacts were deleted" >&2
  fi
}

trap cleanup_on_exit EXIT

model_toml="$(jq -Rn --arg value "$MODEL" '$value')"
catalog_toml="$(jq -Rn --arg value "$CATALOG" '$value')"
base_url_toml="$(jq -Rn --arg value "$BASE_URL" '$value')"

CONFIG_ARGS=(
  -c "model=$model_toml"
  -c 'model_provider="llmconduit"'
  -c "model_catalog_json=$catalog_toml"
  -c 'personality="none"'
  # Codex 0.144.4 otherwise sends a disabled `web_search` declaration with
  # `external_web_access:false` even when the checked model catalog advertises
  # no search tool. The gateway intentionally rejects unsupported hosted-search
  # controls, so keep the controlled smoke aligned with the catalog contract.
  -c 'web_search="disabled"'
  -c 'model_reasoning_effort="max"'
  -c 'model_reasoning_summary="auto"'
  -c 'model_providers.llmconduit.name="Local llmconduit smoke"'
  -c "model_providers.llmconduit.base_url=$base_url_toml"
  -c 'model_providers.llmconduit.env_key="LLMCONDUIT_API_TOKEN"'
  -c 'model_providers.llmconduit.wire_api="responses"'
  -c 'model_providers.llmconduit.requires_openai_auth=false'
)

artifact_contains_secret() {
  local candidate
  for candidate in \
    "$LLMCONDUIT_API_TOKEN" \
    "$PROMPT_CACHE_SENTINEL" \
    "${OPENAI_API_KEY:-}" \
    "${CHATGPT_API_KEY:-}"; do
    [[ -n "$candidate" ]] || continue
    if grep -R -F -q --exclude='.gateway-auth-header' -- "$candidate" "$ARTIFACT_DIR"; then
      return 0
    fi
  done
  return 1
}

scan_for_secrets() {
  if artifact_contains_secret; then
    restore_workspace_permissions
    rm -f "$AUTH_HEADER_FILE"
    rm -rf "$ARTIFACT_DIR"
    die "sensitive test data appeared in an artifact; all smoke artifacts were deleted"
  fi
}

assert_clean_protocol() {
  local name="$1"
  local diagnostics=("$ARTIFACT_DIR/$name.jsonl" "$ARTIFACT_DIR/$name.stderr")
  if grep -E -i -q \
    'missing field [`]?models|model metadata for .* not found|failed to (load|parse|decode|deserialize).*(catalog|response)|response protocol error|invalid responses? event|unexpected responses? event|duplicate sequence|sequence number.*(missing|invalid)|unknown response item' \
    "${diagnostics[@]}"; then
    die "$name emitted a catalog or Responses protocol warning; inspect ${diagnostics[*]}"
  fi
}

run_case() {
  local name="$1"
  local expected_message="$2"
  local expected_commands="$3"
  local prompt="$4"
  local message_match="${5:-exact}"
  local jsonl="$ARTIFACT_DIR/$name.jsonl"
  local stderr="$ARTIFACT_DIR/$name.stderr"
  local last_message="$ARTIFACT_DIR/$name.last-message.txt"

  (
    # Do not rely on an `env` executable: some developer machines install a
    # dotenv-compatible wrapper earlier on PATH whose `-u` behavior differs
    # from coreutils. A subshell makes credential removal explicit and local.
    unset OPENAI_API_KEY CHATGPT_API_KEY
    export CODEX_HOME="$CODEX_HOME_DIR"
    timeout "$TIMEOUT_SECONDS" "$CODEX_BIN" exec \
      --ignore-user-config \
      --ignore-rules \
      --strict-config \
      --ephemeral \
      --json \
      --color never \
      --sandbox read-only \
      --skip-git-repo-check \
      -C "$WORKSPACE" \
      --output-last-message "$last_message" \
      "${CONFIG_ARGS[@]}" \
      "$prompt"
  ) >"$jsonl" 2>"$stderr" || die "$name failed; inspect $jsonl and $stderr"

  [[ -f "$last_message" ]] || die "$name produced no final message; inspect $jsonl and $stderr"
  if [[ "$message_match" == "contains" ]]; then
    [[ "$(<"$last_message")" == *"$expected_message"* ]] || \
      die "$name final message omitted the expected marker; inspect $last_message"
  else
    [[ "$(<"$last_message")" == "$expected_message" ]] || \
      die "$name returned an unexpected final message; inspect $last_message"
  fi
  if jq -e 'select(.type == "item.completed" and .item.type == "error")' "$jsonl" >/dev/null; then
    die "$name emitted a Codex error item; inspect $jsonl"
  fi
  jq -e 'select(.type == "turn.completed")' "$jsonl" >/dev/null || \
    die "$name did not emit turn.completed"
  jq -e '
    select(.type == "turn.completed") |
    .usage | select(type == "object")
  ' "$jsonl" >/dev/null || die "$name did not report turn usage"

  local actual_commands
  actual_commands="$(jq -s '[.[] | select(.type == "item.completed" and .item.type == "command_execution")] | length' "$jsonl")"
  [[ "$actual_commands" == "$expected_commands" ]] || \
    die "$name completed $actual_commands shell commands; expected $expected_commands"

  assert_clean_protocol "$name"
  scan_for_secrets
  echo "codex smoke: $name passed"
}

post_responses() {
  local output_file="$1"
  local payload="$2"

  printf '%s' "$payload" | curl \
    --silent \
    --show-error \
    --max-time "$TIMEOUT_SECONDS" \
    --request POST \
    --url "$BASE_URL/responses" \
    --header "@$AUTH_HEADER_FILE" \
    --header 'content-type: application/json' \
    --header 'accept: application/json' \
    --data-binary @- \
    --output "$output_file" \
    --write-out '%{http_code}'
}

response_output_text() {
  jq -r \
    '[.output[]? | select(.type == "message") | .content[]? | select(.type == "output_text") | .text] | join("")' \
    "$1"
}

assert_usage_shape() {
  local response_file="$1"
  jq -e '
    .usage.input_tokens | numbers
  ' "$response_file" >/dev/null || die "missing numeric input token usage in $response_file"
  jq -e '
    .usage.output_tokens | numbers
  ' "$response_file" >/dev/null || die "missing numeric output token usage in $response_file"
  jq -e '
    .usage.total_tokens | numbers
  ' "$response_file" >/dev/null || die "missing numeric total token usage in $response_file"
  jq -e '
    .usage.input_tokens_details.cached_tokens | numbers
  ' "$response_file" >/dev/null || die "missing numeric cached-token detail in $response_file"
  jq -e '
    .usage.output_tokens_details.reasoning_tokens | numbers
  ' "$response_file" >/dev/null || die "missing numeric reasoning-token detail in $response_file"
}

run_case \
  plain \
  PLAIN_OK \
  0 \
  'Do not use any tools. Reply with exactly: PLAIN_OK'

run_case \
  one-command \
  TOOL_OK \
  1 \
  'Use the shell exactly once to run pwd. After the command succeeds, reply with exactly: TOOL_OK'

run_case \
  two-commands \
  SEQUENTIAL_OK \
  2 \
  'Run exactly two separate shell commands in this order: pwd, then uname -s. Do not combine them. After both succeed, reply with exactly: SEQUENTIAL_OK'

mapfile -t commands < <(
  jq -r 'select(.type == "item.completed" and .item.type == "command_execution") | .item.command' \
    "$ARTIFACT_DIR/two-commands.jsonl"
)
[[ "${commands[0]:-}" == *pwd* ]] || die "the first sequential command was not pwd"
[[ "${commands[1]:-}" == *'uname -s'* ]] || die "the second sequential command was not uname -s"
[[ "${commands[0]}" != "${commands[1]}" ]] || die "the sequential commands were unexpectedly identical"

run_case \
  multi-step \
  MULTISTEP_ALPHA_BETA \
  2 \
  'Use two separate shell invocations. First read alpha.txt. Then read beta.txt. Confirm that their first lines are alpha and beta. Include the marker MULTISTEP_ALPHA_BETA in your final response.' \
  contains

mapfile -t multi_step_commands < <(
  jq -r 'select(.type == "item.completed" and .item.type == "command_execution") | .item.command' \
    "$ARTIFACT_DIR/multi-step.jsonl"
)
[[ "${multi_step_commands[0]:-}" == *alpha.txt* ]] || die "multi-step did not read alpha.txt first"
[[ "${multi_step_commands[1]:-}" == *beta.txt* ]] || die "multi-step did not read beta.txt second"
[[ "${multi_step_commands[0]}" != "${multi_step_commands[1]}" ]] || \
  die "multi-step unexpectedly repeated an identical shell invocation"

parent_payload="$(jq -cn \
  --arg model "$MODEL" \
  --arg marker "$STATE_MARKER" \
  --arg cache_key "$PROMPT_CACHE_SENTINEL" \
  '{
    model: $model,
    store: true,
    stream: false,
    prompt_cache_key: $cache_key,
    input: ("Remember the exact state marker " + $marker + " for the next request. Do not repeat the marker now. Reply with exactly: PARENT_STORED")
  }')"
parent_status="$(post_responses "$ARTIFACT_DIR/state-parent.json" "$parent_payload")"
[[ "$parent_status" == "200" ]] || die "state parent returned HTTP $parent_status"
jq -e '.status == "completed" and .store == true' "$ARTIFACT_DIR/state-parent.json" >/dev/null || \
  die "state parent did not return a completed stored Response"
jq -e --arg cache_key "$PROMPT_CACHE_SENTINEL" '.prompt_cache_key == $cache_key' \
  "$ARTIFACT_DIR/state-parent.json" >/dev/null || \
  die "state parent did not echo the requested prompt_cache_key"
# `prompt_cache_key` is an official client-visible response field, so its echo is
# not a logging leak. Keep the retained smoke artifact secret-free after asserting
# the wire contract; the later recursive scan still catches the sentinel anywhere
# else in Codex output, errors, or other direct Responses fields.
jq '.prompt_cache_key = "[redacted: asserted client echo]"' \
  "$ARTIFACT_DIR/state-parent.json" >"$ARTIFACT_DIR/.state-parent.redacted.json"
mv "$ARTIFACT_DIR/.state-parent.redacted.json" "$ARTIFACT_DIR/state-parent.json"
[[ "$(response_output_text "$ARTIFACT_DIR/state-parent.json")" == "PARENT_STORED" ]] || \
  die "state parent returned unexpected output"
parent_id="$(jq -er '.id | select(startswith("resp_"))' "$ARTIFACT_DIR/state-parent.json")" || \
  die "state parent returned no Responses id"
assert_usage_shape "$ARTIFACT_DIR/state-parent.json"

child_payload="$(jq -cn \
  --arg model "$MODEL" \
  --arg previous_response_id "$parent_id" \
  '{
    model: $model,
    store: false,
    stream: false,
    previous_response_id: $previous_response_id,
    input: "Reply with exactly the state marker from the preceding stored request."
  }')"
child_status="$(post_responses "$ARTIFACT_DIR/state-child.json" "$child_payload")"
[[ "$child_status" == "200" ]] || die "state child returned HTTP $child_status"
jq -e '.status == "completed" and .store == false' "$ARTIFACT_DIR/state-child.json" >/dev/null || \
  die "state child did not return a completed unstored Response"
[[ "$(response_output_text "$ARTIFACT_DIR/state-child.json")" == "$STATE_MARKER" ]] || \
  die "previous_response_id continuation did not recover the state marker"
jq -e --arg parent_id "$parent_id" '.previous_response_id == $parent_id' \
  "$ARTIFACT_DIR/state-child.json" >/dev/null || die "state child did not echo previous_response_id"
assert_usage_shape "$ARTIFACT_DIR/state-child.json"
echo "codex smoke: previous_response_id continuation passed"

service_tier_payload="$(jq -cn \
  --arg model "$MODEL" \
  '{model: $model, store: false, stream: false, service_tier: "priority", input: "Reply with exactly: unreachable"}')"
service_tier_status="$(post_responses "$ARTIFACT_DIR/unsupported-service-tier.json" "$service_tier_payload")"
[[ "$service_tier_status" == "400" ]] || \
  die "unsupported service tier returned HTTP $service_tier_status instead of 400"
jq -e '
  .error.type == "invalid_request_error" and
  .error.param == "service_tier" and
  .error.code == "unsupported_parameter" and
  (.error.message | type == "string" and length > 0)
' "$ARTIFACT_DIR/unsupported-service-tier.json" >/dev/null || \
  die "unsupported service tier did not return a complete OpenAI error"
echo "codex smoke: unsupported service tier passed"

rm -f "$AUTH_HEADER_FILE"
scan_for_secrets

for case_name in plain one-command two-commands multi-step; do
  jq -s \
    'map(select(.type == "turn.completed")) | last | .usage // null' \
    "$ARTIFACT_DIR/$case_name.jsonl" >"$ARTIFACT_DIR/$case_name.usage.json"
done

jq -n \
  --slurpfile plain "$ARTIFACT_DIR/plain.usage.json" \
  --slurpfile one_command "$ARTIFACT_DIR/one-command.usage.json" \
  --slurpfile two_commands "$ARTIFACT_DIR/two-commands.usage.json" \
  --slurpfile multi_step "$ARTIFACT_DIR/multi-step.usage.json" \
  --slurpfile parent "$ARTIFACT_DIR/state-parent.json" \
  --slurpfile child "$ARTIFACT_DIR/state-child.json" \
  '{
    codex: {
      plain: $plain[0],
      one_command: $one_command[0],
      two_commands: $two_commands[0],
      multi_step: $multi_step[0]
    },
    direct_responses: {
      state_parent: {
        usage: $parent[0].usage,
        cached_tokens: $parent[0].usage.input_tokens_details.cached_tokens,
        reasoning_tokens: $parent[0].usage.output_tokens_details.reasoning_tokens
      },
      state_child: {
        usage: $child[0].usage,
        cached_tokens: $child[0].usage.input_tokens_details.cached_tokens,
        reasoning_tokens: $child[0].usage.output_tokens_details.reasoning_tokens
      }
    }
  }' >"$ARTIFACT_DIR/usage-summary.json"
rm -f "$ARTIFACT_DIR"/*.usage.json

OBSERVATION_JSON="${LLMCONDUIT_CODEX_SMOKE_OBSERVATION_JSON:-}"
if [[ -n "$OBSERVATION_JSON" ]]; then
  [[ -f "$OBSERVATION_JSON" ]] || die "observation JSON not found: $OBSERVATION_JSON"
  if grep -F -q -- "$LLMCONDUIT_API_TOKEN" "$OBSERVATION_JSON"; then
    die "external observation contains the raw gateway token"
  fi
  gateway_token_sha256="$(printf '%s' "$LLMCONDUIT_API_TOKEN" | sha256sum)"
  gateway_token_sha256="${gateway_token_sha256%% *}"
  bearer_token_sha256="$(printf 'Bearer %s' "$LLMCONDUIT_API_TOKEN" | sha256sum)"
  bearer_token_sha256="${bearer_token_sha256%% *}"
  jq -e \
    --arg token_hash "$gateway_token_sha256" \
    --arg bearer_hash "$bearer_token_sha256" '
      .schema_version == 1 and
      (.cases | type == "array" and length > 0) and
      (all(.cases[];
        (.name | type == "string" and length > 0) and
        (.raw_usage | type == "object") and
        (.served_usage | type == "object") and
        (.raw_usage == .served_usage) and
        (.raw_call_identities | type == "array") and
        (.served_call_identities | type == "array") and
        (.raw_call_identities == .served_call_identities)
      )) and
      (.upstream_request_headers | type == "array" and length > 0) and
      (all(.upstream_request_headers[];
        (.name | type == "string") and
        (.value_sha256 | type == "string" and test("^[0-9a-f]{64}$")) and
        .value_sha256 != $token_hash and
        .value_sha256 != $bearer_hash
      ))
    ' "$OBSERVATION_JSON" >/dev/null || \
    die "external observation found a usage/call mismatch, gateway credential forwarding, or invalid schema"
  jq '{schema_version, cases, upstream_request_headers}' \
    "$OBSERVATION_JSON" >"$ARTIFACT_DIR/external-observation.json"
  echo "codex smoke: external raw/served and header observation passed"
else
  jq -n '{
    status: "not_run",
    reason: "client-visible Codex artifacts cannot observe raw upstream chunks or upstream request headers",
    hook: "LLMCONDUIT_CODEX_SMOKE_OBSERVATION_JSON"
  }' >"$ARTIFACT_DIR/external-observation.json"
fi

scan_for_secrets

echo "codex smoke: all cases passed"
echo "codex smoke: sanitized artifacts: $ARTIFACT_DIR"
