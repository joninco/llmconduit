# Responses conformance fixtures

These fixtures pin the public OpenAI Responses contract used by
`tests/responses_conformance.rs`. The baseline was reviewed against the official OpenAI Responses
API reference and streaming-event reference on **2026-07-14**:

- <https://platform.openai.com/docs/api-reference/responses>
- <https://platform.openai.com/docs/api-reference/responses-streaming>

They are intentionally small and deterministic. Dynamic identifiers, timestamps, token counts,
provider output, and error messages are asserted structurally by the Rust tests instead of being
copied into brittle golden files. These fixtures describe public wire behavior only; canonical
gateway-only events and fields are not part of the expected surface.

## Represented request schemas

The conformance suite covers string input, typed and easy messages, `developer`/`system`/`user`/
`assistant` history, function calls and outputs, reasoning items, stored item references,
`previous_response_id`, sampling/output controls, metadata, tools, tool choice, structured text
formats, include values, prompt-cache controls, image/file capability gates, and vendor-extension
normalization. `request-string.json` is the minimal standard request used as the starting fixture.

Known unsupported hosted services and capability-gated fields are represented by table-driven
error tests. Those tests require JSON content type and all four OpenAI error fields:
`message`, `type`, `param`, and `code`.

## Represented response resource schemas

Terminal and intermediate resource assertions cover stable `response.id`/`created_at`, status and
completion timestamps, nullability, requested/served model context, instructions, output items,
metadata, reasoning/text controls, tool declarations and selection, sampling limits, truncation,
service tier, `previous_response_id`, `incomplete_details`, and detailed usage. Output coverage
includes messages with `output_text` and refusals, reasoning summary items, function calls, and
web-search calls exposed only through supported include values.

Usage assertions preserve upstream `input_tokens`, `output_tokens`, and `total_tokens`, and require
numeric `input_tokens_details.cached_tokens` and `output_tokens_details.reasoning_tokens` values.
Gateway-internal terminal reasons, stop sequences, token estimates, signatures, and search-result
transport events must not appear on this public surface.

## Represented streaming events

`text-event-order.json` pins the minimal successful text lifecycle. The Rust suite additionally
asserts the complete function-call argument, reasoning-summary, and refusal lifecycles, strictly
increasing zero-based `sequence_number` values, stable IDs/indexes, full terminal resource
snapshots, and exactly one terminal event followed by EOF.

The represented public event families are:

- `response.created`, `response.in_progress`, `response.completed`, `response.incomplete`, and
  `response.failed`;
- `response.output_item.added` and `response.output_item.done`;
- `response.content_part.added` and `response.content_part.done`;
- `response.output_text.delta` and `response.output_text.done`;
- `response.refusal.delta` and `response.refusal.done`;
- reasoning-summary part/text added, delta, done, and item completion; and
- `response.function_call_arguments.delta` and `response.function_call_arguments.done`.

Malformed or prematurely terminated upstream streams are expected to end in one
`response.failed` event without emitting false `done` events. `length` and `content_filter` map to
`response.incomplete`; normal stops and tool handoff map to `response.completed`.

When the official API changes, update this date, refresh only the affected fixtures, and add or
adjust an executable assertion in `tests/responses_conformance.rs`. Do not accept new wire fields
solely by updating a golden file.
