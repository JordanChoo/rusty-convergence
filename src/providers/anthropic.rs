use serde_json::json;

use super::{ProviderError, SseEvent, StreamChunk};
use crate::types::UsageStats;

const PROTECTED_FIELDS: &[&str] = &["model", "stream", "messages", "system"];

pub fn build_request_body(
    model: &str,
    system_prompt: Option<&str>,
    user_prompt: &str,
    provider_params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut body = json!({
        "model": model,
        "max_tokens": 32000,
        "stream": true,
        "messages": [{"role": "user", "content": user_prompt}],
    });

    if let Some(sys) = system_prompt {
        body["system"] = json!(sys);
    }

    if let Some(params) = provider_params {
        if let Some(obj) = params.as_object() {
            if let Some(body_obj) = body.as_object_mut() {
                for (k, v) in obj {
                    if !PROTECTED_FIELDS.contains(&k.as_str()) {
                        body_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }

    body
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockType {
    Text,
    Thinking,
    Unknown,
}

#[derive(Debug, Default)]
pub struct AnthropicStreamState {
    pub current_block: Option<BlockType>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

pub fn parse_event(
    event: &SseEvent,
    state: &mut AnthropicStreamState,
) -> Result<StreamChunk, ProviderError> {
    let event_type = event.event_type.as_deref().unwrap_or("");

    match event_type {
        "message_start" => {
            let parsed: serde_json::Value = serde_json::from_str(&event.data)
                .map_err(|e| ProviderError::ParseError(format!("Invalid JSON: {e}")))?;
            if let Some(usage) = parsed.get("message").and_then(|m| m.get("usage")) {
                state.input_tokens = usage["input_tokens"].as_u64();
            }
            Ok(StreamChunk::Text(String::new()))
        }

        "content_block_start" => {
            let parsed: serde_json::Value = serde_json::from_str(&event.data)
                .map_err(|e| ProviderError::ParseError(format!("Invalid JSON: {e}")))?;
            let block_type = parsed.get("content_block").and_then(|b| b["type"].as_str());
            state.current_block = Some(match block_type {
                Some("text") => BlockType::Text,
                Some("thinking") => BlockType::Thinking,
                _ => BlockType::Unknown,
            });
            Ok(StreamChunk::Text(String::new()))
        }

        "content_block_delta" => {
            if state.current_block.as_ref() != Some(&BlockType::Text) {
                return Ok(StreamChunk::Text(String::new()));
            }

            let parsed: serde_json::Value = serde_json::from_str(&event.data)
                .map_err(|e| ProviderError::ParseError(format!("Invalid JSON: {e}")))?;

            if let Some(text) = parsed.get("delta").and_then(|d| d["text"].as_str()) {
                Ok(StreamChunk::Text(text.to_string()))
            } else {
                Ok(StreamChunk::Text(String::new()))
            }
        }

        "content_block_stop" => {
            state.current_block = None;
            Ok(StreamChunk::Text(String::new()))
        }

        "message_delta" => {
            let parsed: serde_json::Value = serde_json::from_str(&event.data)
                .map_err(|e| ProviderError::ParseError(format!("Invalid JSON: {e}")))?;
            if let Some(usage) = parsed.get("usage") {
                state.output_tokens = usage["output_tokens"].as_u64();
            }
            Ok(StreamChunk::Text(String::new()))
        }

        "message_stop" => Ok(StreamChunk::Done),

        "error" => {
            let parsed: serde_json::Value = serde_json::from_str(&event.data).unwrap_or_default();
            let msg = parsed
                .get("error")
                .and_then(|e| e["message"].as_str())
                .unwrap_or("Unknown error");
            Err(ProviderError::HttpError {
                status: 500,
                body: msg.to_string(),
            })
        }

        _ => Ok(StreamChunk::Text(String::new())),
    }
}

pub fn finalize_usage(state: &AnthropicStreamState) -> UsageStats {
    UsageStats {
        input_tokens: state.input_tokens,
        output_tokens: state.output_tokens,
        reasoning_tokens: None,
    }
}

pub const API_URL: &str = "https://api.anthropic.com/v1/messages";
pub const API_VERSION: &str = "2023-06-01";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_request_with_system() {
        let body = build_request_body("claude-opus-4-6", Some("Be helpful"), "Hello", None);
        assert_eq!(body["system"], "Be helpful");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn test_build_request_without_system() {
        let body = build_request_body("claude-opus-4-6", None, "Hello", None);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn test_provider_params_cannot_override_protected_fields() {
        let params = json!({
            "stream": false,
            "messages": [],
            "model": "hacked-model",
            "system": "hacked system prompt",
            "thinking": {"type": "enabled", "budget_tokens": 32000}
        });
        let body = build_request_body("claude-opus-4-6", Some("real system"), "Hello", Some(&params));
        assert_eq!(body["stream"], true, "stream must not be overridable");
        assert_eq!(body["model"], "claude-opus-4-6", "model must not be overridable");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "messages must not be overridable");
        assert_eq!(body["system"], "real system", "system must not be overridable via params");
        assert_eq!(body["thinking"]["type"], "enabled", "non-protected params should work");
    }

    #[test]
    fn test_build_request_with_thinking() {
        let params = json!({"thinking": {"type": "enabled", "budget_tokens": 32000}});
        let body = build_request_body("claude-opus-4-6", None, "Hello", Some(&params));
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn test_parse_message_start() {
        let event = SseEvent {
            event_type: Some("message_start".into()),
            data: r#"{"message":{"id":"msg_1","usage":{"input_tokens":100}}}"#.into(),
        };
        let mut state = AnthropicStreamState::default();
        let chunk = parse_event(&event, &mut state).unwrap();
        assert!(matches!(chunk, StreamChunk::Text(t) if t.is_empty()));
        assert_eq!(state.input_tokens, Some(100));
    }

    #[test]
    fn test_parse_text_block() {
        let mut state = AnthropicStreamState::default();

        let start = SseEvent {
            event_type: Some("content_block_start".into()),
            data: r#"{"content_block":{"type":"text"}}"#.into(),
        };
        parse_event(&start, &mut state).unwrap();
        assert_eq!(state.current_block, Some(BlockType::Text));

        let delta = SseEvent {
            event_type: Some("content_block_delta".into()),
            data: r#"{"delta":{"type":"text_delta","text":"Hello"}}"#.into(),
        };
        match parse_event(&delta, &mut state).unwrap() {
            StreamChunk::Text(t) => assert_eq!(t, "Hello"),
            other => panic!("Expected Text, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_thinking_block_discarded() {
        let mut state = AnthropicStreamState::default();

        let start = SseEvent {
            event_type: Some("content_block_start".into()),
            data: r#"{"content_block":{"type":"thinking"}}"#.into(),
        };
        parse_event(&start, &mut state).unwrap();
        assert_eq!(state.current_block, Some(BlockType::Thinking));

        let delta = SseEvent {
            event_type: Some("content_block_delta".into()),
            data: r#"{"delta":{"type":"thinking_delta","thinking":"internal..."}}"#.into(),
        };
        match parse_event(&delta, &mut state).unwrap() {
            StreamChunk::Text(t) => assert!(t.is_empty()),
            other => panic!("Expected empty Text, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_message_delta_usage() {
        let event = SseEvent {
            event_type: Some("message_delta".into()),
            data: r#"{"usage":{"output_tokens":250}}"#.into(),
        };
        let mut state = AnthropicStreamState::default();
        parse_event(&event, &mut state).unwrap();
        assert_eq!(state.output_tokens, Some(250));
    }

    #[test]
    fn test_parse_message_stop() {
        let event = SseEvent {
            event_type: Some("message_stop".into()),
            data: "{}".into(),
        };
        let mut state = AnthropicStreamState::default();
        match parse_event(&event, &mut state).unwrap() {
            StreamChunk::Done => {}
            other => panic!("Expected Done, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_error_event() {
        let event = SseEvent {
            event_type: Some("error".into()),
            data: r#"{"error":{"type":"overloaded_error","message":"Overloaded"}}"#.into(),
        };
        let mut state = AnthropicStreamState::default();
        assert!(parse_event(&event, &mut state).is_err());
    }

    #[test]
    fn test_finalize_usage() {
        let state = AnthropicStreamState {
            current_block: None,
            input_tokens: Some(100),
            output_tokens: Some(250),
        };
        let usage = finalize_usage(&state);
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(250));
        assert!(usage.reasoning_tokens.is_none());
    }

    #[test]
    fn test_content_block_stop_resets_state() {
        let mut state = AnthropicStreamState {
            current_block: Some(BlockType::Text),
            ..Default::default()
        };
        let event = SseEvent {
            event_type: Some("content_block_stop".into()),
            data: "{}".into(),
        };
        parse_event(&event, &mut state).unwrap();
        assert!(state.current_block.is_none());
    }
}
