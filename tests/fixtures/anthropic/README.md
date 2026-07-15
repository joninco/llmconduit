# Anthropic Messages request fixtures

These fixtures are synthetic, credential-free request shapes for the
`/v1/messages` and `/v1/messages/count_tokens` conformance tests. They are based
on the public Anthropic Messages contract and on aggregate shapes observed from
Claude Code traffic, but contain no captured prompts, tool descriptions,
arguments, identifiers, or secrets.

The request suite deliberately covers the two common Claude Code tool-catalog
sizes (73 and 90 tools) and a 104-tool/235-message stress case. Tool schemas
exercise the vocabulary seen in practice, including `$schema`, `$defs`, `$ref`,
`propertyNames`, schema-valued `additionalProperties`, annotations, and vendor
extensions. Non-strict Anthropic client tools must reach the OpenAI-compatible
upstream as `strict:false` functions with structurally identical schemas;
separate tests cover official `strict:true` tools.

The fixtures are data, not golden responses. Tests assert the semantic lowering
and public Anthropic response/error envelopes so harmless ID or timestamp
changes do not cause churn.

Contract reference reviewed 2026-07-14:
<https://platform.claude.com/docs/en/api/messages/create>
