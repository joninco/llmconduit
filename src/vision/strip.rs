//! Request-mutation seam for the image agent: strip images to `[Image #N]`
//! placeholders, cache the originals, and inject the `analyzeImage` tool +
//! system-prompt instruction.
//!
//! This is the SINGLE place a [`ResponsesRequest`] is rewritten for the image
//! agent. It runs BEFORE replay/lowering (`Gateway::stream_responses`) so replay
//! hashes only ever see placeholder text, never image bytes. The `analyzeImage`
//! tool schema and the gating predicate ([`latest_user_message_has_images`])
//! that the engine consults to decide activation also live here, alongside the
//! [`ImageCache::strip_and_cache_images`] method that ties them together.

use crate::models::responses::ContentItem;
use crate::models::responses::ResponseItem;
use crate::models::responses::ResponsesRequest;
use crate::models::responses::ToolSpec;
use serde_json::Value;
use serde_json::json;

use super::cache::CachedImage;
use super::cache::ImageCache;

/// Server-side tool name the image agent injects and intercepts. Lowercased
/// once for the case-insensitive registry/lookup parity with `web_search`.
pub const ANALYZE_IMAGE_TOOL_NAME: &str = "analyzeImage";

/// System-prompt block prepended when the image agent is active, instructing the
/// (image-blind) text model to call `analyzeImage` before answering. Adapted
/// from claude-relay's `IMAGE_AGENT_SYSTEM_PROMPT`.
pub const IMAGE_AGENT_SYSTEM_PROMPT: &str = "CRITICAL INSTRUCTION — IMAGE HANDLING:\n\
You CANNOT see images. All images are replaced with [Image #N] placeholders which are OPAQUE to you.\n\
When the latest user message contains [Image #N], you MUST call the `analyzeImage` tool \
as your FIRST action BEFORE generating any text response.\n\
- You have NO ability to see, interpret, or guess what is in an image without calling analyzeImage.\n\
- If you respond about an image without calling analyzeImage first, your response WILL be wrong.\n\
- Call analyzeImage with ALL image IDs from the latest message.\n\
This is non-negotiable. ALWAYS call analyzeImage when [Image #N] is present.";

/// Description for the injected `analyzeImage` tool (claude-relay's
/// `ANALYZE_IMAGE_TOOL` description).
pub const ANALYZE_IMAGE_TOOL_DESCRIPTION: &str = "MANDATORY tool for viewing images. You CANNOT \
see images directly — [Image #N] placeholders are opaque to you. The ONLY way to see image content \
is by calling this tool. You MUST call analyzeImage BEFORE writing ANY response when the latest \
user message contains [Image #N]. NEVER describe, guess, or comment on image content without \
calling this tool first.";

/// JSON-schema parameters for the injected `analyzeImage` tool.
pub fn analyze_image_tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "imageId": {
                "type": "array",
                "description": "Image IDs to analyze — extract from [Image #N] placeholders (e.g. [Image #1] → \"1\")",
                "items": { "type": "string" }
            },
            "task": {
                "type": "string",
                "description": "What to look for in the image, based on conversation context"
            },
            "context": {
                "type": "string",
                "description": "Brief conversation context so the vision model knows what to focus on"
            }
        },
        "required": ["imageId", "task"]
    })
}

/// The canonical `ToolSpec` for the injected `analyzeImage` tool. It lowers to a
/// chat function tool like any other; the server-side classification happens via
/// `ToolKind::ImageAnalysis` (registered only on active image-agent turns).
pub fn analyze_image_tool_spec() -> ToolSpec {
    ToolSpec::Function {
        name: ANALYZE_IMAGE_TOOL_NAME.to_string(),
        description: ANALYZE_IMAGE_TOOL_DESCRIPTION.to_string(),
        strict: false,
        parameters: analyze_image_tool_parameters(),
    }
}

/// Placeholder text that replaces a stripped image. Numbered sequentially within
/// a single request, matching claude-relay's `[Image #N]` format so the model is
/// told exactly how to reference the image when calling `analyzeImage`.
fn image_placeholder_text(number: usize) -> String {
    format!(
        "[Image #{number}] — YOU CANNOT SEE THIS IMAGE. Call analyzeImage(imageId=[\"{number}\"]) to view it."
    )
}

impl ImageCache {
    /// Strip images from the request, cache the originals, inject the
    /// `analyzeImage` tool + system-prompt instruction, and dedup any
    /// caller-supplied `analyzeImage` tool.
    ///
    /// This is the SINGLE mutation surface (called from `Gateway::stream_responses`
    /// BEFORE replay/lowering) so replay hashes only ever see placeholder text,
    /// never image bytes. Walks ALL user messages so a multi-turn history is
    /// renumbered consistently; the cache is cleared first so numbering resets
    /// per request exactly like claude-relay's stateless processing.
    ///
    /// `gate` decides activation: the caller must have already confirmed the
    /// latest user turn has images and the backend is not native-vision. We
    /// renumber every user-message image (not just the latest) because the
    /// history the client resends still contains earlier raw images that would
    /// otherwise leak bytes upstream.
    pub fn strip_and_cache_images(&self, request: &mut ResponsesRequest, session_id: &str) {
        self.clear_session(session_id);
        let mut counter = 1usize;
        for item in &mut request.input {
            let ResponseItem::Message { role, content, .. } = item else {
                continue;
            };
            if role != "user" {
                continue;
            }
            for part in content.iter_mut() {
                if let ContentItem::InputImage {
                    image_url: Some(image_url),
                    detail,
                    ..
                } = part
                {
                    let key = Self::image_key(session_id, &counter.to_string());
                    self.store(
                        session_id,
                        key,
                        CachedImage {
                            image_url: std::mem::take(image_url),
                            detail: detail.clone(),
                        },
                    );
                    *part = ContentItem::InputText {
                        text: image_placeholder_text(counter),
                    };
                    counter += 1;
                }
            }
        }
        inject_analyze_image_tool(&mut request.tools);
        prepend_image_agent_system_prompt(request);
    }
}

/// Install the canonical `analyzeImage` tool for an active image-agent turn,
/// REPLACING any caller-supplied tool of the same name (G4 review #4, hardened
/// round-2 #3) and de-duplicating. The gateway runs `analyzeImage` server-side
/// against the stripped images, so the model MUST see the canonical schema
/// (which requires `imageId`); a conflicting caller schema could make the model
/// omit `imageId` after the images were already replaced with placeholders.
///
/// We drop every existing `analyzeImage` (case-insensitive) BOTH as a top-level
/// Function/Custom tool AND as a NAMESPACED child — a surviving namespaced
/// `analyzeImage` would lower to a chat tool of the same name and collide with
/// the appended canonical tool (`build_tool_registry` rejects duplicate names,
/// case-insensitively). A
/// namespace left empty after removal is dropped entirely. Finally append
/// exactly one canonical spec.
fn inject_analyze_image_tool(tools: &mut Vec<ToolSpec>) {
    tools.retain_mut(|spec| match spec {
        // Top-level analyzeImage (Function/Custom): remove.
        spec if tool_is_analyze_image(spec) => false,
        // Namespaced: strip analyzeImage children; drop the namespace if empty.
        ToolSpec::Namespace { tools, .. } => {
            tools.retain(|tool| !namespace_tool_is_analyze_image(tool));
            !tools.is_empty()
        }
        _ => true,
    });
    tools.push(analyze_image_tool_spec());
}

/// Whether a namespace child tool is `analyzeImage` (case-insensitive).
fn namespace_tool_is_analyze_image(tool: &crate::models::responses::NamespaceToolSpec) -> bool {
    match tool {
        crate::models::responses::NamespaceToolSpec::Function { name, .. } => {
            name.eq_ignore_ascii_case(ANALYZE_IMAGE_TOOL_NAME)
        }
    }
}

/// Whether a `ToolSpec` is the `analyzeImage` tool (case-insensitive on the
/// function name), used for dedup and for relaxing a forced `tool_choice`.
pub fn tool_is_analyze_image(spec: &ToolSpec) -> bool {
    match spec {
        ToolSpec::Function { name, .. } | ToolSpec::Custom { name, .. } => {
            name.eq_ignore_ascii_case(ANALYZE_IMAGE_TOOL_NAME)
        }
        _ => false,
    }
}

/// Prepend the image-agent instruction to `instructions` (the canonical home for
/// system text), mirroring `apply_system_prompt_prefix`'s prepend convention.
fn prepend_image_agent_system_prompt(request: &mut ResponsesRequest) {
    request.instructions = if request.instructions.is_empty() {
        IMAGE_AGENT_SYSTEM_PROMPT.to_string()
    } else {
        format!("{IMAGE_AGENT_SYSTEM_PROMPT}\n\n{}", request.instructions)
    };
}

/// Whether the LATEST user message in the canonical input carries at least one
/// `InputImage` with a usable `image_url`. Only the latest user turn activates
/// the agent (claude-relay's `has_images`): old images lingering in history must
/// not re-trigger stripping/tool-injection.
pub fn latest_user_message_has_images(input: &[ResponseItem]) -> bool {
    let Some(content) = latest_user_message_content(input) else {
        return false;
    };
    content.iter().any(|part| {
        matches!(
            part,
            ContentItem::InputImage {
                image_url: Some(url),
                ..
            } if !url.is_empty()
        )
    })
}

fn latest_user_message_content(input: &[ResponseItem]) -> Option<&[ContentItem]> {
    input.iter().rev().find_map(|item| match item {
        ResponseItem::Message { role, content, .. } if role == "user" => Some(content.as_slice()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn cache() -> ImageCache {
        ImageCache::new(100, std::time::Duration::from_secs(300))
    }

    fn img(url: &str) -> CachedImage {
        CachedImage {
            image_url: url.to_string(),
            detail: None,
        }
    }

    fn user_with(content: Vec<ContentItem>) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content,
            phase: None,
        }
    }

    fn input_image(url: &str) -> ContentItem {
        ContentItem::InputImage {
            image_url: Some(url.to_string()),
            file_id: None,
            detail: None,
        }
    }

    fn base_request(input: Vec<ResponseItem>) -> ResponsesRequest {
        let mut req: ResponsesRequest =
            serde_json::from_value(serde_json::json!({ "model": "glm-5.1" })).expect("request");
        req.input = input;
        req
    }

    #[test]
    fn latest_user_detection_true_for_image_in_last_user() {
        let input = vec![
            user_with(vec![ContentItem::InputText { text: "hi".into() }]),
            ResponseItem::message_text("assistant", "hello"),
            user_with(vec![
                ContentItem::InputText { text: "see".into() },
                input_image("data:img"),
            ]),
        ];
        assert!(latest_user_message_has_images(&input));
    }

    #[test]
    fn latest_user_detection_false_for_only_old_images() {
        let input = vec![
            user_with(vec![input_image("data:old")]),
            ResponseItem::message_text("assistant", "I saw it"),
            user_with(vec![ContentItem::InputText {
                text: "thanks".into(),
            }]),
        ];
        assert!(!latest_user_message_has_images(&input));
    }

    #[test]
    fn latest_user_detection_false_without_images_or_users() {
        assert!(!latest_user_message_has_images(&[]));
        let input = vec![ResponseItem::message_text("assistant", "hi")];
        assert!(!latest_user_message_has_images(&input));
        let empty_image = vec![user_with(vec![ContentItem::InputImage {
            image_url: None,
            file_id: Some("f".into()),
            detail: None,
        }])];
        assert!(!latest_user_message_has_images(&empty_image));
    }

    #[test]
    fn strip_replaces_images_with_ordered_placeholders() {
        let cache = cache();
        let mut req = base_request(vec![user_with(vec![
            ContentItem::InputText {
                text: "Look at".into(),
            },
            input_image("data:img1"),
            ContentItem::InputText { text: "and".into() },
            input_image("data:img2"),
        ])]);
        cache.strip_and_cache_images(&mut req, "sess");
        let ResponseItem::Message { content, .. } = &req.input[0] else {
            panic!("expected message");
        };
        assert!(matches!(&content[0], ContentItem::InputText { text } if text == "Look at"));
        let ContentItem::InputText { text } = &content[1] else {
            panic!("expected placeholder");
        };
        assert!(text.contains("[Image #1]"));
        assert!(text.contains("analyzeImage(imageId=[\"1\"])"));
        let ContentItem::InputText { text } = &content[3] else {
            panic!("expected placeholder");
        };
        assert!(text.contains("[Image #2]"));
        // Originals cached under session-scoped keys.
        assert_eq!(
            cache.get("sess", &ImageCache::image_key("sess", "1")),
            Some(img("data:img1"))
        );
        assert_eq!(
            cache.get("sess", &ImageCache::image_key("sess", "2")),
            Some(img("data:img2"))
        );
    }

    #[test]
    fn strip_injects_system_prompt_and_tool_once() {
        let cache = cache();
        let mut req = base_request(vec![user_with(vec![input_image("data:img")])]);
        cache.strip_and_cache_images(&mut req, "sess");
        assert!(req.instructions.starts_with(IMAGE_AGENT_SYSTEM_PROMPT));
        let analyze_count = req
            .tools
            .iter()
            .filter(|t| tool_is_analyze_image(t))
            .count();
        assert_eq!(analyze_count, 1);
    }

    #[test]
    fn strip_preserves_existing_instructions() {
        let cache = cache();
        let mut req = base_request(vec![user_with(vec![input_image("data:img")])]);
        req.instructions = "Be terse.".to_string();
        cache.strip_and_cache_images(&mut req, "sess");
        assert!(req.instructions.starts_with(IMAGE_AGENT_SYSTEM_PROMPT));
        assert!(req.instructions.ends_with("Be terse."));
    }

    #[test]
    fn strip_does_not_duplicate_existing_analyze_tool() {
        let cache = cache();
        let mut req = base_request(vec![user_with(vec![input_image("data:img")])]);
        req.tools = vec![analyze_image_tool_spec()];
        cache.strip_and_cache_images(&mut req, "sess");
        let analyze_count = req
            .tools
            .iter()
            .filter(|t| tool_is_analyze_image(t))
            .count();
        assert_eq!(analyze_count, 1);
    }

    #[test]
    fn strip_replaces_conflicting_caller_analyze_schema_with_canonical() {
        // A caller-supplied analyzeImage with a conflicting schema (no imageId)
        // must be REPLACED by the canonical spec on activation (review #4), so
        // the model is told to pass imageId after images were stripped.
        let cache = cache();
        let mut req = base_request(vec![user_with(vec![input_image("data:img")])]);
        req.tools = vec![ToolSpec::Function {
            name: "analyzeImage".to_string(),
            description: "caller's own bogus tool".to_string(),
            strict: false,
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "q": { "type": "string" } },
                "required": ["q"]
            }),
        }];
        cache.strip_and_cache_images(&mut req, "sess");
        let analyze: Vec<&ToolSpec> = req
            .tools
            .iter()
            .filter(|t| tool_is_analyze_image(t))
            .collect();
        assert_eq!(
            analyze.len(),
            1,
            "exactly one analyzeImage after replace+dedup"
        );
        let ToolSpec::Function {
            description,
            parameters,
            ..
        } = analyze[0]
        else {
            panic!("expected function tool");
        };
        assert_eq!(description, ANALYZE_IMAGE_TOOL_DESCRIPTION);
        assert_eq!(parameters, &analyze_image_tool_parameters());
        // Canonical schema requires imageId, not the caller's `q`.
        assert_eq!(
            parameters["required"],
            serde_json::json!(["imageId", "task"])
        );
    }

    #[test]
    fn strip_replaces_namespaced_caller_analyze_tool() {
        // Round-2 #3: a NAMESPACED analyzeImage child must also be removed (it
        // would otherwise lower to a chat tool of the same name and collide with
        // the appended canonical tool). The now-empty namespace is dropped.
        let cache = cache();
        let mut req = base_request(vec![user_with(vec![input_image("data:img")])]);
        req.tools = vec![
            ToolSpec::Namespace {
                name: "mcp".to_string(),
                description: "an mcp server".to_string(),
                tools: vec![crate::models::responses::NamespaceToolSpec::Function {
                    name: "analyzeImage".to_string(),
                    description: "namespaced bogus analyzeImage".to_string(),
                    strict: false,
                    parameters: serde_json::json!({ "type": "object", "properties": {} }),
                }],
            },
            ToolSpec::Namespace {
                name: "tools".to_string(),
                description: "another server".to_string(),
                tools: vec![crate::models::responses::NamespaceToolSpec::Function {
                    name: "keep_me".to_string(),
                    description: "unrelated".to_string(),
                    strict: false,
                    parameters: serde_json::json!({ "type": "object", "properties": {} }),
                }],
            },
        ];
        cache.strip_and_cache_images(&mut req, "sess");
        // Exactly one canonical top-level analyzeImage; no namespaced survivor.
        let analyze_top = req
            .tools
            .iter()
            .filter(|t| tool_is_analyze_image(t))
            .count();
        assert_eq!(analyze_top, 1);
        // The empty "mcp" namespace was dropped; "tools" (with keep_me) survives.
        let namespaces: Vec<&str> = req
            .tools
            .iter()
            .filter_map(|t| match t {
                ToolSpec::Namespace { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(namespaces, vec!["tools"]);
        // Lowering must NOT error on a duplicate `analyzeImage` name.
        let lowered = crate::adapters::responses_to_chat::lower_request_with_image_agent(
            &req,
            Vec::new(),
            true,
        );
        assert!(
            lowered.is_ok(),
            "no duplicate-tool error after namespaced replace"
        );
    }

    #[test]
    fn strip_preserves_non_user_messages_and_empty_content() {
        let cache = cache();
        let mut req = base_request(vec![
            ResponseItem::message_text("assistant", "I see it"),
            user_with(vec![]),
            user_with(vec![input_image("data:img")]),
        ]);
        cache.strip_and_cache_images(&mut req, "sess");
        assert!(matches!(&req.input[0], ResponseItem::Message { role, .. } if role == "assistant"));
        let ResponseItem::Message { content, .. } = &req.input[1] else {
            panic!("expected message");
        };
        assert!(content.is_empty());
    }

    #[test]
    fn strip_renumbers_across_all_user_messages() {
        // History resent with raw images across two user turns must renumber
        // 1,2,3 sequentially so no raw bytes survive in any user message.
        let cache = cache();
        let mut req = base_request(vec![
            user_with(vec![input_image("data:a"), input_image("data:b")]),
            ResponseItem::message_text("assistant", "ok"),
            user_with(vec![input_image("data:c")]),
        ]);
        cache.strip_and_cache_images(&mut req, "sess");
        let collect_placeholders = |item: &ResponseItem| -> Vec<String> {
            let ResponseItem::Message { content, .. } = item else {
                return Vec::new();
            };
            content
                .iter()
                .filter_map(|c| match c {
                    ContentItem::InputText { text } if text.contains("[Image #") => {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        let first = collect_placeholders(&req.input[0]);
        let third = collect_placeholders(&req.input[2]);
        assert!(first[0].contains("[Image #1]"));
        assert!(first[1].contains("[Image #2]"));
        assert!(third[0].contains("[Image #3]"));
        assert_eq!(cache.session_len("sess"), 3);
    }

    #[test]
    fn strip_clears_session_so_multi_turn_numbering_resets() {
        // Turn 1: two images -> #1, #2. Turn 2 (fresh request, history carries
        // placeholders, one NEW image) -> cache cleared, new image becomes #1.
        let cache = cache();
        let mut turn1 = base_request(vec![user_with(vec![
            input_image("data:t1a"),
            input_image("data:t1b"),
        ])]);
        cache.strip_and_cache_images(&mut turn1, "sess");
        assert_eq!(cache.session_len("sess"), 2);

        let mut turn2 = base_request(vec![
            // Turn 1 history already stripped to placeholders (text only).
            user_with(vec![
                ContentItem::InputText {
                    text: image_placeholder_text(1),
                },
                ContentItem::InputText {
                    text: image_placeholder_text(2),
                },
            ]),
            ResponseItem::message_text("assistant", "analyzing"),
            user_with(vec![input_image("data:t2new")]),
        ]);
        cache.strip_and_cache_images(&mut turn2, "sess");
        // Only the new image remains, renumbered to #1.
        assert_eq!(cache.session_len("sess"), 1);
        assert_eq!(
            cache.get("sess", &ImageCache::image_key("sess", "1")),
            Some(img("data:t2new"))
        );
        assert_eq!(cache.get("sess", &ImageCache::image_key("sess", "2")), None);
    }
}
