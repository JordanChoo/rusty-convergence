pub mod anthropic;
pub mod openai;

use crate::types::UsageStats;

#[derive(Debug, Clone)]
pub enum StreamChunk {
    Text(String),
    Usage(UsageStats),
    Done,
}

#[derive(Debug)]
pub enum ProviderError {
    HttpError { status: u16, body: String },
    StreamDisconnect { bytes_received: usize },
    EmptyResponse,
    Timeout,
    ParseError(String),
    ConfigError(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HttpError { status, body } => write!(f, "HTTP {status}: {body}"),
            Self::StreamDisconnect { bytes_received } => {
                write!(f, "Stream disconnected after {bytes_received} bytes")
            }
            Self::EmptyResponse => write!(f, "Empty response from provider"),
            Self::Timeout => write!(f, "No data received for 5 minutes"),
            Self::ParseError(msg) => write!(f, "SSE parse error: {msg}"),
            Self::ConfigError(msg) => write!(f, "Provider config error: {msg}"),
        }
    }
}

#[derive(Debug)]
pub struct SseEvent {
    pub event_type: Option<String>,
    pub data: String,
}

pub fn parse_sse_events(raw: &str) -> Vec<SseEvent> {
    let mut events = Vec::new();

    for block in raw.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }

        let mut event_type: Option<String> = None;
        let mut data_lines: Vec<&str> = Vec::new();

        for line in block.lines() {
            if line.starts_with(':') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                event_type = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.trim_start_matches(' '));
            } else if line == "data" {
                data_lines.push("");
            }
        }

        if !data_lines.is_empty() {
            events.push(SseEvent {
                event_type,
                data: data_lines.join("\n"),
            });
        }
    }

    events
}

pub fn error_code_for_provider_error(err: &ProviderError) -> (&'static str, u16) {
    match err {
        ProviderError::HttpError { .. } => ("provider_error", 502),
        ProviderError::Timeout => ("provider_timeout", 504),
        ProviderError::StreamDisconnect { .. } => ("provider_error", 502),
        ProviderError::EmptyResponse => ("provider_error", 502),
        ProviderError::ParseError(_) => ("provider_error", 502),
        ProviderError::ConfigError(_) => ("missing_config", 500),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_event() {
        let events = parse_sse_events("data: hello\n\n");
        assert_eq!(events.len(), 1);
        assert!(events[0].event_type.is_none());
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_parse_event_with_type() {
        let events = parse_sse_events("event: message\ndata: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type.as_deref(), Some("message"));
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_parse_multiline_data() {
        let events = parse_sse_events("data: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn test_parse_comment_skipped() {
        let events = parse_sse_events(":comment\ndata: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_parse_multiple_events() {
        let events = parse_sse_events("data: first\n\ndata: second\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "first");
        assert_eq!(events[1].data, "second");
    }

    #[test]
    fn test_parse_empty() {
        let events = parse_sse_events("");
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_done_marker() {
        let events = parse_sse_events("data: [DONE]\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "[DONE]");
    }

    #[test]
    fn test_provider_error_display() {
        let e = ProviderError::HttpError {
            status: 429,
            body: "rate limited".into(),
        };
        assert_eq!(format!("{e}"), "HTTP 429: rate limited");

        let e = ProviderError::StreamDisconnect {
            bytes_received: 4231,
        };
        assert!(format!("{e}").contains("4231"));
    }

    #[test]
    fn test_error_code_mapping() {
        assert_eq!(
            error_code_for_provider_error(&ProviderError::Timeout),
            ("provider_timeout", 504)
        );
        assert_eq!(
            error_code_for_provider_error(&ProviderError::EmptyResponse),
            ("provider_error", 502)
        );
        assert_eq!(
            error_code_for_provider_error(&ProviderError::ConfigError("x".into())),
            ("missing_config", 500)
        );
    }
}
