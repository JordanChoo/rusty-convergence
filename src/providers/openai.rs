use serde_json::json;

use super::{ProviderError, SseEvent, StreamChunk};
use crate::types::UsageStats;

pub fn build_request_body(
    model: &str,
    system_prompt: Option<&str>,
    user_prompt: &str,
    provider_params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut messages = Vec::new();
    if let Some(sys) = system_prompt {
        messages.push(json!({"role": "system", "content": sys}));
    }
    messages.push(json!({"role": "user", "content": user_prompt}));

    let mut body = json!({
        "model": model,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });

    if let Some(params) = provider_params {
        if let Some(obj) = params.as_object() {
            if let Some(body_obj) = body.as_object_mut() {
                for (k, v) in obj {
                    body_obj.insert(k.clone(), v.clone());
                }
            }
        }
    }

    body
}

pub fn parse_event(event: &SseEvent) -> Result<StreamChunk, ProviderError> {
    if event.data == "[DONE]" {
        return Ok(StreamChunk::Done);
    }

    let parsed: serde_json::Value = serde_json::from_str(&event.data)
        .map_err(|e| ProviderError::ParseError(format!("Invalid JSON in SSE data: {e}")))?;

    if let Some(usage) = parsed.get("usage") {
        let input_tokens = usage["prompt_tokens"].as_u64();
        let output_tokens = usage["completion_tokens"].as_u64();
        let reasoning_tokens = usage
            .get("completion_tokens_details")
            .and_then(|d| d["reasoning_tokens"].as_u64());

        if input_tokens.is_some() || output_tokens.is_some() {
            return Ok(StreamChunk::Usage(UsageStats {
                input_tokens,
                output_tokens,
                reasoning_tokens,
            }));
        }
    }

    if let Some(choices) = parsed.get("choices") {
        if let Some(delta) = choices.get(0).and_then(|c| c.get("delta")) {
            if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                if !content.is_empty() {
                    return Ok(StreamChunk::Text(content.to_string()));
                }
            }
        }
    }

    Ok(StreamChunk::Text(String::new()))
}

pub const API_URL: &str = "https://api.openai.com/v1/chat/completions";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_request_with_system() {
        let body = build_request_body("o3", Some("You are helpful"), "Hello", None);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn test_build_request_without_system() {
        let body = build_request_body("o3", None, "Hello", None);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn test_build_request_with_provider_params() {
        let params = json!({"reasoning_effort": "high", "max_completion_tokens": 32000});
        let body = build_request_body("o3", None, "Hello", Some(&params));
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_completion_tokens"], 32000);
    }

    #[test]
    fn test_parse_done() {
        let event = SseEvent {
            event_type: None,
            data: "[DONE]".into(),
        };
        match parse_event(&event).unwrap() {
            StreamChunk::Done => {}
            other => panic!("Expected Done, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_content_chunk() {
        let event = SseEvent {
            event_type: None,
            data: r#"{"choices":[{"delta":{"content":"Hello"}}]}"#.into(),
        };
        match parse_event(&event).unwrap() {
            StreamChunk::Text(t) => assert_eq!(t, "Hello"),
            other => panic!("Expected Text, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_usage() {
        let event = SseEvent {
            event_type: None,
            data: r#"{"usage":{"prompt_tokens":100,"completion_tokens":50,"completion_tokens_details":{"reasoning_tokens":30}}}"#.into(),
        };
        match parse_event(&event).unwrap() {
            StreamChunk::Usage(u) => {
                assert_eq!(u.input_tokens, Some(100));
                assert_eq!(u.output_tokens, Some(50));
                assert_eq!(u.reasoning_tokens, Some(30));
            }
            other => panic!("Expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_empty_content() {
        let event = SseEvent {
            event_type: None,
            data: r#"{"choices":[{"delta":{}}]}"#.into(),
        };
        match parse_event(&event).unwrap() {
            StreamChunk::Text(t) => assert!(t.is_empty()),
            other => panic!("Expected empty Text, got {other:?}"),
        }
    }
}
