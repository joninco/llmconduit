use llmconduit::adapters::responses_to_chat::lower_request;
use llmconduit::models::responses::ContentItem;
use llmconduit::models::responses::CustomToolFormat;
use llmconduit::models::responses::FunctionCallOutputContent;
use llmconduit::models::responses::ResponseInstructions;
use llmconduit::models::responses::ResponseItem;
use llmconduit::models::responses::ResponsesRequest;
use llmconduit::models::responses::ToolSpec;
use serde_json::Value;
use serde_json::json;

fn parse_request(body: Value) -> ResponsesRequest {
    serde_json::from_value(body).expect("valid Responses request")
}

fn message_text(item: &ResponseItem) -> (&str, &str) {
    let ResponseItem::Message { role, content, .. } = item else {
        panic!("expected message item, got {item:?}");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected one normalized input_text part, got {content:?}");
    };
    (role, text)
}

#[test]
fn easy_messages_accept_omitted_type_and_string_content() {
    let request = parse_request(json!({
        "model": "test-model",
        "input": [
            { "role": "user", "content": "hello" },
            { "type": "message", "role": "assistant", "content": "hi" },
            { "role": "developer", "content": "be concise" }
        ]
    }));

    assert_eq!(message_text(&request.input[0]), ("user", "hello"));
    assert_eq!(message_text(&request.input[1]), ("assistant", "hi"));
    assert_eq!(message_text(&request.input[2]), ("developer", "be concise"));

    let serialized = serde_json::to_value(&request).expect("serialize request");
    for item in serialized["input"].as_array().expect("input array") {
        assert_eq!(item["type"], "message");
        assert_eq!(item["content"][0]["type"], "input_text");
    }

    let lowered = lower_request(&request, Vec::new()).expect("lower easy messages");
    assert_eq!(
        lowered
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["user", "assistant", "developer"]
    );
}

#[test]
fn bare_string_input_remains_supported() {
    let request = parse_request(json!({
        "model": "test-model",
        "input": "hello"
    }));

    assert_eq!(request.input.len(), 1);
    assert_eq!(message_text(&request.input[0]), ("user", "hello"));
}

#[test]
fn responses_validates_metadata_sampling_and_output_limits() {
    for (temperature, top_p) in [(0.0, 0.0), (2.0, 1.0)] {
        let request = parse_request(json!({
            "model": "test-model",
            "input": "hello",
            "temperature": temperature,
            "top_p": top_p,
            "max_output_tokens": 1
        }));
        lower_request(&request, Vec::new()).expect("inclusive numeric boundaries are valid");
    }

    let mut valid_metadata = serde_json::Map::new();
    for index in 0..14 {
        valid_metadata.insert(format!("key-{index}"), json!(format!("value-{index}")));
    }
    valid_metadata.insert("k".repeat(64), json!("value"));
    valid_metadata.insert("max-value".to_string(), json!("v".repeat(512)));
    let request = parse_request(json!({
        "model": "test-model",
        "input": "hello",
        "metadata": valid_metadata
    }));
    lower_request(&request, Vec::new()).expect("metadata boundaries are valid");

    let mut too_many_metadata = serde_json::Map::new();
    for index in 0..17 {
        too_many_metadata.insert(format!("key-{index}"), json!("value"));
    }
    let cases = [
        (
            json!({"model":"test-model","input":"hello","temperature":-0.01}),
            "temperature".to_string(),
            "invalid_value",
        ),
        (
            json!({"model":"test-model","input":"hello","temperature":2.01}),
            "temperature".to_string(),
            "invalid_value",
        ),
        (
            json!({"model":"test-model","input":"hello","top_p":-0.01}),
            "top_p".to_string(),
            "invalid_value",
        ),
        (
            json!({"model":"test-model","input":"hello","top_p":1.01}),
            "top_p".to_string(),
            "invalid_value",
        ),
        (
            json!({"model":"test-model","input":"hello","max_output_tokens":0}),
            "max_output_tokens".to_string(),
            "invalid_value",
        ),
        (
            json!({"model":"test-model","input":"hello","max_output_tokens":-1}),
            "max_output_tokens".to_string(),
            "invalid_value",
        ),
        (
            json!({"model":"test-model","input":"hello","metadata":too_many_metadata}),
            "metadata".to_string(),
            "invalid_value",
        ),
        (
            json!({"model":"test-model","input":"hello","metadata":{"":"value"}}),
            "metadata.".to_string(),
            "invalid_value",
        ),
        (
            json!({
                "model":"test-model",
                "input":"hello",
                "metadata":{("k".repeat(65)):"value"}
            }),
            format!("metadata.{}", "k".repeat(65)),
            "invalid_value",
        ),
        (
            json!({"model":"test-model","input":"hello","metadata":{"key":42}}),
            "metadata.key".to_string(),
            "invalid_type",
        ),
        (
            json!({
                "model":"test-model",
                "input":"hello",
                "metadata":{"key":"v".repeat(513)}
            }),
            "metadata.key".to_string(),
            "invalid_type",
        ),
    ];

    for (body, expected_param, expected_code) in cases {
        let request = parse_request(body);
        let error = lower_request(&request, Vec::new())
            .expect_err("out-of-range request control must fail before dispatch");
        assert_eq!(error.param.as_deref(), Some(expected_param.as_str()));
        assert_eq!(error.code.as_deref(), Some(expected_code));
    }
}

#[test]
fn instructions_accept_string_and_normalized_item_arrays() {
    let text = parse_request(json!({
        "model": "test-model",
        "instructions": "Be concise.",
        "input": "hello"
    }));
    assert_eq!(
        text.instructions,
        ResponseInstructions::Text("Be concise.".to_string())
    );
    assert_eq!(text.instructions.replay_key(), "Be concise.");
    let text_lowered = lower_request(&text, Vec::new()).expect("lower text instructions");
    assert_eq!(text_lowered.messages[0].role, "system");
    assert_eq!(text_lowered.messages[0].content, Some(json!("Be concise.")));

    let items = parse_request(json!({
        "model": "test-model",
        "instructions": [
            { "role": "developer", "content": "Use JSON." },
            {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "Instruction context" }]
            }
        ],
        "input": [{ "role": "user", "content": "Actual request" }]
    }));
    let ResponseInstructions::Items(instruction_items) = &items.instructions else {
        panic!("expected structured instructions")
    };
    assert!(
        items
            .instructions
            .replay_key()
            .starts_with("\0responses-instruction-items:")
    );
    assert_eq!(
        message_text(&instruction_items[0]),
        ("developer", "Use JSON.")
    );
    assert_eq!(
        message_text(&instruction_items[1]),
        ("user", "Instruction context")
    );

    let serialized = serde_json::to_value(&items).expect("serialize item instructions");
    assert_eq!(serialized["instructions"][0]["type"], "message");
    assert_eq!(
        serialized["instructions"][0]["content"][0],
        json!({ "type": "input_text", "text": "Use JSON." })
    );
    let lowered = lower_request(&items, Vec::new()).expect("lower item instructions");
    assert_eq!(
        lowered
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["developer", "user", "user"]
    );
    assert_eq!(lowered.messages[2].content, Some(json!("Actual request")));
}

#[test]
fn request_serialization_filters_extra_body_typed_key_collisions() {
    let mut request = parse_request(json!({
        "model": "typed-model",
        "input": "hello",
        "temperature": 0.25,
        "vendor_knob": { "enabled": true }
    }));
    request
        .extra_body
        .insert("model".to_string(), json!("shadow-model"));
    request
        .extra_body
        .insert("input".to_string(), json!("shadow"));
    request
        .extra_body
        .insert("temperature".to_string(), json!(1.75));
    request
        .extra_body
        .insert("prompt_cache_retention".to_string(), json!("shadow"));
    request.prompt_cache_retention = Some("24h".to_string());

    let wire = serde_json::to_string(&request).expect("serialize collision-safe request");
    assert_eq!(wire.matches("\"model\":").count(), 1, "{wire}");
    assert_eq!(wire.matches("\"input\":").count(), 1, "{wire}");
    assert_eq!(wire.matches("\"temperature\":").count(), 1, "{wire}");
    assert_eq!(
        wire.matches("\"prompt_cache_retention\":").count(),
        1,
        "{wire}"
    );
    let serialized: Value = serde_json::from_str(&wire).expect("parse serialized request");
    assert_eq!(serialized["model"], "typed-model");
    assert_eq!(serialized["temperature"], 0.25);
    assert_eq!(serialized["prompt_cache_retention"], "24h");
    assert_eq!(serialized["vendor_knob"], json!({ "enabled": true }));
    assert_eq!(message_text(&request.input[0]), ("user", "hello"));
}

#[test]
fn responses_client_metadata_round_trips_as_vendor_extension() {
    let metadata = json!({
        "x-codex-installation-id": "installation-123",
        "traceparent": "00-opaque-trace-opaque-span-01"
    });
    let request = parse_request(json!({
        "model": "test-model",
        "input": "hello",
        "client_metadata": metadata.clone()
    }));

    assert_eq!(request.extra_body.get("client_metadata"), Some(&metadata));
    let serialized = serde_json::to_value(&request).expect("serialize request");
    assert_eq!(serialized["client_metadata"], metadata);

    let roundtripped: ResponsesRequest =
        serde_json::from_value(serialized).expect("deserialize request again");
    assert_eq!(
        roundtripped.extra_body.get("client_metadata"),
        request.extra_body.get("client_metadata")
    );
}

#[test]
fn function_call_output_accepts_only_string_or_content_array() {
    let string_output = parse_request(json!({
        "model":"test-model",
        "input":[{"type":"function_call_output","call_id":"call_1","output":"done"}]
    }));
    assert!(matches!(
        &string_output.input[0],
        ResponseItem::FunctionCallOutput {
            output: FunctionCallOutputContent::Text(text),
            ..
        } if text == "done"
    ));

    let content_output = parse_request(json!({
        "model":"test-model",
        "input":[{"type":"function_call_output","call_id":"call_1","output":[
            {"type":"input_text","text":"see attached"},
            {"type":"input_image","image_url":"https://example.test/a.png"}
        ]}]
    }));
    assert!(matches!(
        &content_output.input[0],
        ResponseItem::FunctionCallOutput {
            output: FunctionCallOutputContent::Content(content),
            ..
        } if matches!(content.as_slice(), [ContentItem::InputText { .. }, ContentItem::InputImage { .. }])
    ));

    for invalid in [json!(42), json!({"result":"done"}), Value::Null] {
        let parsed = serde_json::from_value::<ResponsesRequest>(json!({
            "model":"test-model",
            "input":[{"type":"function_call_output","call_id":"call_1","output":invalid}]
        }));
        assert!(
            parsed.is_err(),
            "invalid function output shape was accepted"
        );
    }
}

#[test]
fn custom_tool_output_uses_the_same_string_or_content_union() {
    let request = parse_request(json!({
        "model":"test-model",
        "input":[
            {"type":"custom_tool_call_output","call_id":"call_1","output":"done"},
            {"type":"custom_tool_call_output","call_id":"call_2","output":[
                {"type":"input_text","text":"see attached"},
                {"type":"input_image","image_url":"https://example.test/a.png"}
            ]}
        ]
    }));
    assert!(matches!(
        &request.input[0],
        ResponseItem::CustomToolCallOutput {
            output: FunctionCallOutputContent::Text(text),
            ..
        } if text == "done"
    ));
    assert!(matches!(
        &request.input[1],
        ResponseItem::CustomToolCallOutput {
            output: FunctionCallOutputContent::Content(content),
            ..
        } if matches!(content.as_slice(), [ContentItem::InputText { .. }, ContentItem::InputImage { .. }])
    ));

    let text_content = parse_request(json!({
        "model":"test-model",
        "input":[{"type":"custom_tool_call_output","call_id":"call_3","output":[
            {"type":"input_text","text":"structured result"}
        ]}]
    }));
    let lowered = lower_request(&text_content, Vec::new()).expect("lower custom output content");
    assert_eq!(lowered.messages[0].role, "tool");
    assert_eq!(
        lowered.messages[0].content,
        Some(json!("structured result"))
    );

    for invalid in [json!(42), json!({"result":"done"}), Value::Null] {
        let parsed = serde_json::from_value::<ResponsesRequest>(json!({
            "model":"test-model",
            "input":[{"type":"custom_tool_call_output","call_id":"call_1","output":invalid}]
        }));
        assert!(parsed.is_err(), "invalid custom output shape was accepted");
    }
}

#[test]
fn flat_and_legacy_function_tool_choice_are_both_accepted() {
    let tool = json!({
        "type": "function",
        "name": "echo",
        "parameters": {
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        },
        "strict": true
    });

    let flat = parse_request(json!({
        "model": "test-model",
        "input": "hello",
        "tools": [tool.clone()],
        "tool_choice": { "type": "function", "name": "echo" },
        "vendor_knob": { "enabled": true }
    }));
    let legacy = parse_request(json!({
        "model": "test-model",
        "input": "hello",
        "tools": [tool],
        "tool_choice": {
            "type": "function",
            "function": { "name": "echo" }
        }
    }));

    let normalized = json!({
        "type": "function",
        "function": { "name": "echo" }
    });
    assert_eq!(flat.tool_choice, normalized);
    assert_eq!(legacy.tool_choice, normalized);
    assert_eq!(flat.extra_body["vendor_knob"], json!({ "enabled": true }));

    let ToolSpec::Function { description, .. } = &flat.tools[0] else {
        panic!("expected function tool");
    };
    assert!(description.is_empty(), "description is optional");

    let flat_lowered = lower_request(&flat, Vec::new()).expect("lower flat tool choice");
    let legacy_lowered = lower_request(&legacy, Vec::new()).expect("lower legacy tool choice");
    assert_eq!(flat_lowered.tools, legacy_lowered.tools);
    assert_eq!(flat_lowered.tools[0].function.description, "");

    let serialized = serde_json::to_value(&flat).expect("serialize request");
    assert_eq!(serialized["vendor_knob"], json!({ "enabled": true }));
    assert!(serialized["tools"][0].get("description").is_none());
}

#[test]
fn custom_tools_accept_official_defaults_formats_and_forced_choice() {
    let omitted = parse_request(json!({
        "model": "test-model",
        "input": "hello",
        "tools": [{ "type": "custom", "name": "apply_patch" }],
        "tool_choice": { "type": "custom", "name": "apply_patch" }
    }));
    assert!(matches!(
        &omitted.tools[0],
        ToolSpec::Custom {
            description,
            format: CustomToolFormat::Text,
            ..
        } if description.is_empty()
    ));
    let omitted_lowered = lower_request(&omitted, Vec::new()).expect("lower default text custom");
    assert!(
        omitted_lowered.tools[0]
            .function
            .description
            .contains("unconstrained string")
    );

    let explicit_text = parse_request(json!({
        "model": "test-model",
        "input": "hello",
        "tools": [{
            "type": "custom",
            "name": "plain_text",
            "description": "Produce plain text",
            "format": { "type": "text" }
        }]
    }));
    assert!(matches!(
        &explicit_text.tools[0],
        ToolSpec::Custom {
            format: CustomToolFormat::Text,
            ..
        }
    ));

    let grammar = parse_request(json!({
        "model": "test-model",
        "input": "hello",
        "tools": [{
            "type": "custom",
            "name": "regex_tool",
            "format": {
                "type": "grammar",
                "syntax": "regex",
                "definition": "[a-z]+"
            }
        }]
    }));
    let grammar_lowered = lower_request(&grammar, Vec::new()).expect("lower grammar custom");
    assert!(
        grammar_lowered.tools[0]
            .function
            .description
            .contains("[a-z]+")
    );

    let serialized = serde_json::to_value(&omitted).expect("serialize custom tool");
    assert_eq!(serialized["tools"][0]["format"], json!({ "type": "text" }));
    assert_eq!(
        serialized["tool_choice"],
        json!({ "type": "custom", "name": "apply_patch" })
    );
}

#[test]
fn custom_tool_validation_rejects_invalid_names_grammars_and_selector_kinds() {
    let cases = [
        (
            json!({ "type":"custom", "name":"bad name", "format":{"type":"text"} }),
            "tools[0].name",
        ),
        (
            json!({
                "type":"custom", "name":"bad_syntax",
                "format":{"type":"grammar", "syntax":"abnf", "definition":"start"}
            }),
            "tools[0].format.syntax",
        ),
        (
            json!({
                "type":"custom", "name":"empty_grammar",
                "format":{"type":"grammar", "syntax":"lark", "definition":"  "}
            }),
            "tools[0].format.definition",
        ),
    ];
    for (tool, expected_param) in cases {
        let request = parse_request(json!({
            "model":"test-model", "input":"hello", "tools":[tool]
        }));
        let error = lower_request(&request, Vec::new()).expect_err("invalid custom tool");
        assert_eq!(error.param.as_deref(), Some(expected_param));
    }

    let custom_as_function = parse_request(json!({
        "model":"test-model",
        "input":"hello",
        "tools":[{"type":"custom","name":"apply_patch"}],
        "tool_choice":{"type":"function","name":"apply_patch"}
    }));
    assert!(lower_request(&custom_as_function, Vec::new()).is_err());

    let function_as_custom = parse_request(json!({
        "model":"test-model",
        "input":"hello",
        "tools":[{
            "type":"function", "name":"echo", "strict":false, "parameters":{}
        }],
        "tool_choice":{"type":"custom","name":"echo"}
    }));
    assert!(lower_request(&function_as_custom, Vec::new()).is_err());

    let duplicate = parse_request(json!({
        "model":"test-model",
        "input":"hello",
        "tools":[
            {"type":"custom","name":"duplicate"},
            {"type":"custom","name":"DUPLICATE","format":{"type":"text"}}
        ]
    }));
    assert!(lower_request(&duplicate, Vec::new()).is_err());
}

#[test]
fn responses_function_strict_defaults_true_without_changing_explicit_false() {
    let omitted = parse_request(json!({
        "model":"test-model",
        "input":"hello",
        "tools":[{
            "type":"function",
            "name":"echo",
            "parameters":{
                "type":"object",
                "properties":{"value":{"type":"string"}},
                "required":["value"],
                "additionalProperties":false
            }
        }]
    }));
    let ToolSpec::Function { strict, .. } = &omitted.tools[0] else {
        panic!("expected function tool")
    };
    assert!(*strict);
    assert!(
        lower_request(&omitted, Vec::new()).unwrap().tools[0]
            .function
            .strict
    );

    let explicit_false = parse_request(json!({
        "model":"test-model",
        "input":"hello",
        "tools":[{"type":"function","name":"echo","strict":false,"parameters":{}}]
    }));
    let ToolSpec::Function { strict, .. } = &explicit_false.tools[0] else {
        panic!("expected function tool")
    };
    assert!(!*strict);
    assert!(
        !lower_request(&explicit_false, Vec::new()).unwrap().tools[0]
            .function
            .strict
    );
}

#[test]
fn text_format_union_accepts_and_round_trips_all_variants() {
    let cases = [
        json!({ "type": "text" }),
        json!({ "type": "json_object" }),
        json!({
            "type": "json_schema",
            "name": "answer",
            "description": "A machine-readable answer.",
            "schema": {
                "type": "object",
                "properties": { "answer": { "type": "integer" } },
                "required": ["answer"],
                "additionalProperties": false
            },
            "strict": true
        }),
    ];

    for expected in cases {
        let request = parse_request(json!({
            "model": "test-model",
            "input": "hello",
            "text": { "format": expected.clone() }
        }));
        let format = request
            .text
            .as_ref()
            .and_then(|text| text.format.as_ref())
            .expect("text format");
        assert_eq!(format.kind, expected["type"]);

        let serialized = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(serialized["text"]["format"], expected);

        let lowered = lower_request(&request, Vec::new()).expect("lower text format");
        assert_eq!(
            lowered.response_format.as_ref().expect("response format")["type"],
            format.kind
        );
        if format.kind == "json_schema" {
            assert_eq!(
                lowered.response_format.as_ref().expect("response format")["json_schema"]["description"],
                "A machine-readable answer."
            );
        }
    }
}

#[test]
fn strict_schema_validation_is_recursive_and_validates_numeric_constraints() {
    let valid = parse_request(json!({
        "model": "test-model",
        "input": "hello",
        "tools": [{
            "type": "function",
            "name": "inspect",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {
                    "payload": {
                        "type": "object",
                        "properties": {
                            "count": { "type": "integer", "description": "Item count" }
                        },
                        "required": ["count"],
                        "additionalProperties": false
                    }
                },
                "required": ["payload"],
                "additionalProperties": false
            }
        }]
    }));
    lower_request(&valid, Vec::new()).expect("supported recursive strict schema");

    let mut invalid = valid;
    let ToolSpec::Function { parameters, .. } = &mut invalid.tools[0] else {
        panic!("expected function tool");
    };
    parameters["properties"]["payload"]["properties"]["count"]["minimum"] = json!(0);
    parameters["properties"]["payload"]["properties"]["count"]["maximum"] = json!(10);
    lower_request(&invalid, Vec::new()).expect("standard numeric constraints are supported");

    let ToolSpec::Function { parameters, .. } = &mut invalid.tools[0] else {
        panic!("expected function tool");
    };
    parameters["properties"]["payload"]["properties"]["count"]["minimum"] = json!("zero");
    let error = lower_request(&invalid, Vec::new())
        .expect_err("an invalid numeric constraint must fail before dispatch");
    assert_eq!(error.code.as_deref(), Some("invalid_json_schema"));
    assert_eq!(
        error.param.as_deref(),
        Some("tools[0].parameters.properties.payload.properties.count.minimum")
    );
}

#[test]
fn strict_schema_supports_defs_refs_any_of_nullable_types_and_constraints() {
    let request = parse_request(json!({
        "model": "test-model",
        "input": "hello",
        "tools": [{
            "type": "function",
            "name": "inspect",
            "strict": true,
            "parameters": {
                "type": "object",
                "$defs": {
                    "result": {
                        "type": "object",
                        "properties": {
                            "count": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 100,
                                "multipleOf": 1
                            },
                            "id": {
                                "type": ["string", "null"],
                                "minLength": 3,
                                "maxLength": 36,
                                "pattern": "^[a-z0-9-]+$"
                            }
                        },
                        "required": ["count", "id"],
                        "additionalProperties": false
                    }
                },
                "properties": {
                    "result": {
                        "anyOf": [
                            { "$ref": "#/$defs/result" },
                            { "type": "null" }
                        ]
                    },
                    "labels": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "minItems": 1,
                        "maxItems": 3,
                        "uniqueItems": true
                    }
                },
                "required": ["result", "labels"],
                "additionalProperties": false
            }
        }]
    }));

    lower_request(&request, Vec::new()).expect("official strict-schema constructs are supported");

    let mut unresolved = request;
    let ToolSpec::Function { parameters, .. } = &mut unresolved.tools[0] else {
        panic!("expected function tool");
    };
    parameters["properties"]["result"]["anyOf"][0]["$ref"] = json!("#/$defs/missing");
    let error = lower_request(&unresolved, Vec::new())
        .expect_err("an unresolved local reference must fail before dispatch");
    assert_eq!(error.code.as_deref(), Some("invalid_json_schema"));
    assert_eq!(
        error.param.as_deref(),
        Some("tools[0].parameters.properties.result.anyOf[0].$ref")
    );
}

#[test]
fn non_strict_function_schemas_validate_syntax_and_supported_vocabulary() {
    let valid = parse_request(json!({
        "model": "test-model",
        "input": "hello",
        "tools": [{
            "type": "function",
            "name": "configure",
            "strict": false,
            "parameters": {
                "properties": {
                    "env": {
                        "type": "object",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["env"]
            }
        }]
    }));
    lower_request(&valid, Vec::new())
        .expect("valid permissive non-strict schemas must remain supported");

    for (schema, expected_param) in [
        (json!({ "type": 42 }), "tools[0].parameters.type"),
        (
            json!({ "type": "object", "properties": [] }),
            "tools[0].parameters.properties",
        ),
        (
            json!({
                "type": "object",
                "properties": { "count": { "type": "integer", "minimum": "zero" } }
            }),
            "tools[0].parameters.properties.count.minimum",
        ),
        (
            json!({ "type": "object", "oneOf": [{ "type": "object" }] }),
            "tools[0].parameters.oneOf",
        ),
    ] {
        let mut invalid = valid.clone();
        let ToolSpec::Function { parameters, .. } = &mut invalid.tools[0] else {
            panic!("expected function tool");
        };
        *parameters = schema;
        let error = lower_request(&invalid, Vec::new())
            .expect_err("malformed or unsupported non-strict schema must fail locally");
        assert_eq!(error.code.as_deref(), Some("invalid_json_schema"));
        assert_eq!(error.param.as_deref(), Some(expected_param));
    }
}

#[test]
fn json_schema_format_requires_name_and_schema_and_rejects_unknown_types() {
    for invalid in [
        json!({ "type": "json_schema", "name": "answer" }),
        json!({ "type": "json_schema", "schema": {} }),
        json!({ "type": "xml" }),
    ] {
        let result = serde_json::from_value::<ResponsesRequest>(json!({
            "model": "test-model",
            "input": "hello",
            "text": { "format": invalid }
        }));
        assert!(result.is_err(), "invalid text format must be rejected");
    }
}
