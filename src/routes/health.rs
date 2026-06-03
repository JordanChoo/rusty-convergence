use serde_json::json;
use worker::*;

use crate::error::{json_error, success_response, VERSION};
use crate::validation::{validate_auth, validate_csvkey};

pub async fn handle(env: Env, url: &Url) -> Result<Response> {
    let kv_accessible = env.kv("APRP").is_ok();
    let mut data = json!({
        "version": VERSION,
        "kv_accessible": kv_accessible,
    });
    let mut warnings = Vec::new();

    if health_auth_requested(url) {
        let csvkey = match validate_csvkey(url) {
            Ok(k) => k,
            Err(resp) => return Ok(resp),
        };
        let expected = match env.secret("CSVKEY") {
            Ok(s) => s.to_string(),
            Err(_) => {
                console_log!("missing_secret: CSVKEY");
                return json_error(500, "Server configuration error", "missing_config", None);
            }
        };
        if let Err(resp) = validate_auth(&csvkey, &expected) {
            return Ok(resp);
        }

        let (diagnostics, diagnostic_warnings) = secret_diagnostics(
            secret_is_configured(&env, "OPENAI_API_KEY"),
            secret_is_configured(&env, "ANTHROPIC_API_KEY"),
        );
        if let Some(fields) = data.as_object_mut() {
            fields.insert("diagnostics".to_string(), diagnostics);
        }
        warnings.extend(diagnostic_warnings);
    }

    success_response(data, warnings, None)
}

fn health_auth_requested(url: &Url) -> bool {
    url.query_pairs().any(|(key, _)| key.eq("csvkey"))
}

fn secret_is_configured(env: &Env, name: &str) -> bool {
    match env.secret(name) {
        Ok(secret) => !secret.to_string().trim().is_empty(),
        Err(_) => false,
    }
}

fn secret_diagnostics(
    openai_configured: bool,
    anthropic_configured: bool,
) -> (serde_json::Value, Vec<String>) {
    let mut warnings = Vec::new();
    if !openai_configured {
        warnings.push("OPENAI_API_KEY Worker secret is not configured".to_string());
    }
    if !anthropic_configured {
        warnings.push("ANTHROPIC_API_KEY Worker secret is not configured".to_string());
    }

    (
        json!({
            "auth": "ok",
            "secrets": {
                "CSVKEY": true,
                "OPENAI_API_KEY": openai_configured,
                "ANTHROPIC_API_KEY": anthropic_configured,
            },
            "providers": {
                "openai": {
                    "configured": openai_configured,
                    "required_secret": "OPENAI_API_KEY",
                },
                "anthropic": {
                    "configured": anthropic_configured,
                    "required_secret": "ANTHROPIC_API_KEY",
                },
            },
        }),
        warnings,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(condition: bool, message: &str) -> std::result::Result<(), String> {
        if condition {
            Ok(())
        } else {
            Err(message.to_string())
        }
    }

    fn path_value<'a>(
        value: &'a serde_json::Value,
        path: &[&str],
    ) -> std::result::Result<&'a serde_json::Value, String> {
        path.iter().try_fold(value, |current, segment| {
            current
                .get(*segment)
                .ok_or_else(|| format!("missing JSON path {}", path.join(".")))
        })
    }

    fn check_path_str(
        value: &serde_json::Value,
        path: &[&str],
        expected: &str,
    ) -> std::result::Result<(), String> {
        let actual = path_value(value, path)?;
        let name = path.join(".");
        match actual.as_str() {
            Some(actual) if actual.eq(expected) => Ok(()),
            _ => Err(format!("expected {name} to be {expected}, got {actual}")),
        }
    }

    fn check_path_bool(
        value: &serde_json::Value,
        path: &[&str],
        expected: bool,
    ) -> std::result::Result<(), String> {
        let actual = path_value(value, path)?;
        let name = path.join(".");
        match (actual.as_bool(), expected) {
            (Some(true), true) | (Some(false), false) => Ok(()),
            _ => Err(format!("expected {name} to be {expected}, got {actual}")),
        }
    }

    #[test]
    fn test_health_auth_requested_detects_csvkey_presence() -> std::result::Result<(), String> {
        let public = Url::parse("https://example.test/health").map_err(|e| e.to_string())?;
        let authenticated =
            Url::parse("https://example.test/health?csvkey=secret").map_err(|e| e.to_string())?;
        let empty_auth =
            Url::parse("https://example.test/health?csvkey=").map_err(|e| e.to_string())?;

        check(
            !health_auth_requested(&public),
            "public health should not request auth",
        )?;
        check(
            health_auth_requested(&authenticated),
            "csvkey value should request auth",
        )?;
        check(
            health_auth_requested(&empty_auth),
            "empty csvkey should still request auth",
        )
    }

    #[test]
    fn test_secret_diagnostics_reports_all_configured() -> std::result::Result<(), String> {
        let (diagnostics, warnings) = secret_diagnostics(true, true);

        check(
            warnings.is_empty(),
            "no warnings expected when all secrets exist",
        )?;
        check_path_str(&diagnostics, &["auth"], "ok")?;
        check_path_bool(&diagnostics, &["secrets", "CSVKEY"], true)?;
        check_path_bool(&diagnostics, &["secrets", "OPENAI_API_KEY"], true)?;
        check_path_bool(&diagnostics, &["secrets", "ANTHROPIC_API_KEY"], true)?;
        check_path_bool(&diagnostics, &["providers", "openai", "configured"], true)?;
        check_path_bool(
            &diagnostics,
            &["providers", "anthropic", "configured"],
            true,
        )
    }

    #[test]
    fn test_secret_diagnostics_warns_for_missing_provider_secrets(
    ) -> std::result::Result<(), String> {
        let (diagnostics, warnings) = secret_diagnostics(false, true);

        check(
            matches!(
                warnings.as_slice(),
                [warning] if warning.as_str().eq("OPENAI_API_KEY Worker secret is not configured")
            ),
            "missing OpenAI secret should produce one warning",
        )?;
        check_path_bool(&diagnostics, &["secrets", "OPENAI_API_KEY"], false)?;
        check_path_bool(&diagnostics, &["secrets", "ANTHROPIC_API_KEY"], true)?;
        check_path_bool(&diagnostics, &["providers", "openai", "configured"], false)?;
        check_path_bool(
            &diagnostics,
            &["providers", "anthropic", "configured"],
            true,
        )
    }
}
