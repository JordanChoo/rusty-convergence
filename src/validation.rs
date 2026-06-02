use worker::*;

use crate::error::json_error;

pub fn validate_csvkey(url: &Url) -> std::result::Result<String, Response> {
    let params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    params
        .get("csvkey")
        .filter(|v| !v.is_empty())
        .cloned()
        .ok_or_else(|| {
            json_error(
                401,
                "Missing required parameter: csvkey",
                "missing_csvkey",
                None,
            )
            .unwrap()
        })
}

pub fn validate_auth(provided: &str, expected: &str) -> std::result::Result<(), Response> {
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(json_error(401, "Unauthorized", "unauthorized", None).unwrap());
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[derive(Debug)]
pub struct ValidationError {
    pub message: String,
    pub code: String,
}

impl ValidationError {
    pub fn into_response(self) -> Response {
        json_error(400, &self.message, &self.code, None).unwrap()
    }
}

pub fn check_workflow_name(name: &str) -> std::result::Result<(), ValidationError> {
    if name.is_empty() || name.len() > 64 {
        return Err(ValidationError {
            message: "Workflow name must be 1-64 characters".into(),
            code: "bad_request".into(),
        });
    }
    let valid = name
        .chars()
        .next()
        .map_or(false, |c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err(ValidationError {
            message: "Workflow name must start with alphanumeric and contain only alphanumeric, hyphens, or underscores".into(),
            code: "bad_request".into(),
        });
    }
    Ok(())
}

pub fn validate_workflow_name(name: &str) -> std::result::Result<(), Response> {
    check_workflow_name(name).map_err(|e| e.into_response())
}

pub fn check_role_name(role: &str) -> std::result::Result<(), ValidationError> {
    if role.is_empty() || role.len() > 32 {
        return Err(ValidationError {
            message: "Document role must be 1-32 characters".into(),
            code: "bad_request".into(),
        });
    }
    let valid = role
        .chars()
        .next()
        .map_or(false, |c| c.is_ascii_alphanumeric())
        && role
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err(ValidationError {
            message: "Document role must start with alphanumeric and contain only alphanumeric, hyphens, or underscores".into(),
            code: "bad_request".into(),
        });
    }
    Ok(())
}

pub fn validate_role_name(role: &str) -> std::result::Result<(), Response> {
    check_role_name(role).map_err(|e| e.into_response())
}

pub fn check_round_number(round: u32) -> std::result::Result<(), ValidationError> {
    if round == 0 || round > 999 {
        return Err(ValidationError {
            message: "Round number must be between 1 and 999".into(),
            code: "bad_request".into(),
        });
    }
    Ok(())
}

#[allow(dead_code)]
pub fn validate_round_number(round: u32) -> std::result::Result<(), Response> {
    check_round_number(round).map_err(|e| e.into_response())
}

pub fn parse_and_validate_round(round_str: &str) -> std::result::Result<u32, Response> {
    let round: u32 = round_str.parse().map_err(|_| {
        json_error(
            400,
            &format!("Invalid round number: '{round_str}'. Must be a positive integer."),
            "bad_request",
            None,
        )
        .unwrap()
    })?;
    check_round_number(round).map_err(|e| e.into_response())?;
    Ok(round)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"secret123", b"secret123"));
    }

    #[test]
    fn test_constant_time_eq_different() {
        assert!(!constant_time_eq(b"secret123", b"secret456"));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"short", b"longer_string"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_workflow_name_valid() {
        assert!(check_workflow_name("fcp-spec").is_ok());
        assert!(check_workflow_name("my_workflow").is_ok());
        assert!(check_workflow_name("a").is_ok());
        assert!(check_workflow_name("123start").is_ok());
        assert!(check_workflow_name(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn test_workflow_name_invalid() {
        assert!(check_workflow_name("").is_err());
        assert!(check_workflow_name("-start").is_err());
        assert!(check_workflow_name("hello world").is_err());
        assert!(check_workflow_name(&"a".repeat(65)).is_err());
        assert!(check_workflow_name("_start").is_err());
    }

    #[test]
    fn test_role_name_valid() {
        assert!(check_role_name("readme").is_ok());
        assert!(check_role_name("spec").is_ok());
        assert!(check_role_name("my-impl").is_ok());
        assert!(check_role_name(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn test_role_name_invalid() {
        assert!(check_role_name("").is_err());
        assert!(check_role_name(&"a".repeat(33)).is_err());
        assert!(check_role_name("-start").is_err());
    }

    #[test]
    fn test_round_number_valid() {
        assert!(check_round_number(1).is_ok());
        assert!(check_round_number(500).is_ok());
        assert!(check_round_number(999).is_ok());
    }

    #[test]
    fn test_round_number_invalid() {
        assert!(check_round_number(0).is_err());
        assert!(check_round_number(1000).is_err());
    }

    #[test]
    fn test_validation_error_has_correct_code() {
        let err = check_workflow_name("").unwrap_err();
        assert_eq!(err.code, "bad_request");

        let err = check_round_number(0).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }
}
