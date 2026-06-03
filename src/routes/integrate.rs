use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::json;
use worker::kv::KvStore;
use worker::*;

use crate::error::{json_error, success_response};
use crate::storage::{doc_key, kv_get, kv_get_text, kv_put_text, round_key};
use crate::types::{Round, RoundStatus, UsageStats};
use crate::validation::{parse_and_validate_round, validate_workflow_name};

pub async fn handle(kv: KvStore, workflow: &str, round_str: &str) -> Result<Response> {
    if let Err(resp) = validate_workflow_name(workflow) {
        return Ok(resp);
    }
    let round = match parse_and_validate_round(round_str) {
        Ok(n) => n,
        Err(resp) => return Ok(resp),
    };

    let key = round_key(workflow, round);
    let record = match kv_get::<Round>(&kv, &key).await? {
        Some(r) => r,
        None => {
            return json_error(
                404,
                &format!("Round {round} does not exist for workflow '{workflow}'"),
                "not_found",
                Some(&format!(
                    "Use POST /run/{workflow}/{round} to create this round."
                )),
            )
        }
    };

    if record.status != RoundStatus::Complete {
        return json_error(
            422,
            &format!(
                "Round {round} has status '{}'; only completed rounds can be integrated",
                serde_json::to_value(&record.status)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("unknown")
            ),
            "validation_failed",
            None,
        );
    }

    let content = record.content.unwrap_or_default();
    let prompt = build_integration_prompt(workflow, round, &content);

    success_response(
        json!({
            "workflow": workflow,
            "round": round,
            "prompt": prompt,
        }),
        vec![],
        Some("Provide the current spec and README alongside this prompt to give the coding agent full context."),
    )
}

pub fn build_integration_prompt(workflow: &str, round: u32, content: &str) -> String {
    format!(
        "The following are revision suggestions from round {round} of iterative specification \
         review for the \"{workflow}\" specification. Apply each suggested change to the \
         specification document, preserving the existing structure where possible. For each \
         change, explain what you modified and why.\n\n---\n\n{content}"
    )
}

// --- Claude document integration (APR-style document mutation) ---

pub const DEFAULT_INTEGRATION_MODEL: &str = "claude-sonnet-4-6";

const INTEGRATION_SYSTEM_PROMPT: &str = "\
You are a meticulous document editor. You integrate review feedback into documents \
by applying suggested improvements directly. You output only the updated document \
with no explanations or meta-commentary.";

pub fn build_document_integration_prompt(
    role: &str,
    round_content: &str,
    document_content: &str,
) -> String {
    format!(
        "You received output from an iterative specification review. Apply all relevant \
         improvements from the review to update this document.\n\n\
         <review>\n{round_content}\n</review>\n\n\
         <document role=\"{role}\">\n{document_content}\n</document>\n\n\
         Rules:\n\
         - Apply all improvements from the review that are relevant to this document\n\
         - Make the document read as if it was always written this way\n\
         - Do not reference \"changes\", \"updates\", or the review process\n\
         - If the review has no suggestions relevant to this document, return it unchanged\n\
         - Return ONLY the complete updated document content, with no additional commentary"
    )
}

#[derive(Debug)]
pub enum IntegrationError {
    MissingApiKey,
    DocumentNotFound(String),
    ProviderError(String),
    KvError(String),
}

impl std::fmt::Display for IntegrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingApiKey => write!(f, "Missing ANTHROPIC_API_KEY for integration"),
            Self::DocumentNotFound(role) => write!(f, "Document not found for role '{role}'"),
            Self::ProviderError(msg) => write!(f, "Integration provider error: {msg}"),
            Self::KvError(msg) => write!(f, "Integration KV error: {msg}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationResult {
    pub documents_updated: Vec<String>,
    pub usage: UsageStats,
    pub duration_seconds: u64,
}

pub async fn integrate_documents_claude(
    kv: &KvStore,
    env: &Env,
    workflow: &str,
    round_content: &str,
    documents: &HashMap<String, String>,
    integration_model: &str,
) -> std::result::Result<IntegrationResult, IntegrationError> {
    let api_key = env
        .secret("ANTHROPIC_API_KEY")
        .map_err(|_| IntegrationError::MissingApiKey)?
        .to_string();

    let start_ms = Date::now().as_millis();
    let mut documents_updated = Vec::new();
    let mut total_usage = UsageStats {
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
    };

    for (role, role_id) in documents {
        let doc_content = kv_get_text(kv, &doc_key(workflow, role_id))
            .await
            .map_err(|e| IntegrationError::KvError(e.to_string()))?
            .ok_or_else(|| IntegrationError::DocumentNotFound(role.clone()))?;

        let user_prompt = build_document_integration_prompt(role, round_content, &doc_content);

        let (updated_content, call_usage) =
            call_claude_non_streaming(&api_key, integration_model, &user_prompt).await?;

        kv_put_text(kv, &doc_key(workflow, role_id), &updated_content)
            .await
            .map_err(|e| IntegrationError::KvError(e.to_string()))?;

        documents_updated.push(role.clone());

        total_usage.input_tokens =
            Some(total_usage.input_tokens.unwrap_or(0) + call_usage.input_tokens.unwrap_or(0));
        total_usage.output_tokens =
            Some(total_usage.output_tokens.unwrap_or(0) + call_usage.output_tokens.unwrap_or(0));
    }

    let duration_seconds = (Date::now().as_millis() - start_ms) / 1000;
    documents_updated.sort();

    Ok(IntegrationResult {
        documents_updated,
        usage: total_usage,
        duration_seconds,
    })
}

async fn call_claude_non_streaming(
    api_key: &str,
    model: &str,
    user_prompt: &str,
) -> std::result::Result<(String, UsageStats), IntegrationError> {
    let body = json!({
        "model": model,
        "max_tokens": 32000,
        "system": INTEGRATION_SYSTEM_PROMPT,
        "messages": [{"role": "user", "content": user_prompt}],
    });

    let headers = Headers::new();
    headers
        .set("x-api-key", api_key)
        .map_err(|e| IntegrationError::ProviderError(e.to_string()))?;
    headers
        .set(
            "anthropic-version",
            crate::providers::anthropic::API_VERSION,
        )
        .map_err(|e| IntegrationError::ProviderError(e.to_string()))?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|e| IntegrationError::ProviderError(e.to_string()))?;

    let mut fetch_init = RequestInit::new();
    fetch_init.with_method(Method::Post);
    fetch_init.with_headers(headers);
    fetch_init.with_body(Some(wasm_bindgen::JsValue::from_str(&body.to_string())));

    let fetch_request = Request::new_with_init(crate::providers::anthropic::API_URL, &fetch_init)
        .map_err(|e| IntegrationError::ProviderError(e.to_string()))?;

    let mut response = Fetch::Request(fetch_request)
        .send()
        .await
        .map_err(|e| IntegrationError::ProviderError(format!("Fetch failed: {e}")))?;

    let status = response.status_code();
    if status != 200 {
        let error_body = response.text().await.unwrap_or_default();
        let truncated = if error_body.len() > 2048 {
            &error_body[..error_body.floor_char_boundary(2048)]
        } else {
            &error_body
        };
        return Err(IntegrationError::ProviderError(format!(
            "HTTP {status}: {truncated}"
        )));
    }

    let response_json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| IntegrationError::ProviderError(format!("Invalid JSON response: {e}")))?;

    let text = response_json["content"]
        .as_array()
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b["type"].as_str() == Some("text"))
                .and_then(|b| b["text"].as_str())
        })
        .ok_or_else(|| {
            IntegrationError::ProviderError("No text content in integration response".into())
        })?;

    let usage = UsageStats {
        input_tokens: response_json
            .get("usage")
            .and_then(|u| u["input_tokens"].as_u64()),
        output_tokens: response_json
            .get("usage")
            .and_then(|u| u["output_tokens"].as_u64()),
        reasoning_tokens: None,
    };

    Ok((text.to_string(), usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_integration_prompt_includes_role_and_content() {
        let prompt = build_document_integration_prompt("spec", "Fix the API design", "# My Spec");
        assert!(prompt.contains("<review>"));
        assert!(prompt.contains("Fix the API design"));
        assert!(prompt.contains("role=\"spec\""));
        assert!(prompt.contains("# My Spec"));
        assert!(prompt.contains("Return ONLY"));
    }

    #[test]
    fn integration_error_display() {
        let err = IntegrationError::MissingApiKey;
        assert!(err.to_string().contains("ANTHROPIC_API_KEY"));

        let err = IntegrationError::DocumentNotFound("readme".into());
        assert!(err.to_string().contains("readme"));
    }
}
