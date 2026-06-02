use serde_json::json;
use worker::*;

pub const VERSION: &str = "0.1.0";
const MAX_ERROR_BODY_BYTES: usize = 4096;

fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn json_error(
    status: u16,
    error_msg: &str,
    code: &str,
    hint: Option<&str>,
) -> Result<Response> {
    let body = json!({
        "ok": false,
        "code": code,
        "data": null,
        "warnings": [],
        "hint": hint,
        "error": truncate(error_msg, MAX_ERROR_BODY_BYTES),
        "meta": {
            "version": VERSION,
            "ts": now_iso8601()
        }
    });
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    Ok(Response::ok(body.to_string())?
        .with_headers(headers)
        .with_status(status))
}

pub fn success_response(
    data: serde_json::Value,
    warnings: Vec<String>,
    hint: Option<&str>,
) -> Result<Response> {
    let body = json!({
        "ok": true,
        "code": "ok",
        "data": data,
        "warnings": warnings,
        "hint": hint,
        "error": null,
        "meta": {
            "version": VERSION,
            "ts": now_iso8601()
        }
    });
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    Response::ok(body.to_string()).map(|r| r.with_headers(headers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_at_limit() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long_string() {
        assert_eq!(truncate("hello world", 5), "hello");
    }

    #[test]
    fn test_truncate_unicode_boundary() {
        let s = "héllo";
        let truncated = truncate(s, 2);
        assert_eq!(truncated, "h");
    }

    #[test]
    fn test_truncate_empty() {
        assert_eq!(truncate("", 10), "");
    }
}
