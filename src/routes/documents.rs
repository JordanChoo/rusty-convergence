use serde_json::json;
use worker::kv::KvStore;
use worker::*;

use crate::error::{json_error, success_response};
use crate::storage::{doc_key, kv_get_text, kv_put_text};
use crate::validation::validate_role_name;

const DEFAULT_MAX_DOCUMENT_BYTES: usize = 1_048_576; // 1 MB
const SMALL_DOCUMENT_THRESHOLD: usize = 500;

fn max_document_bytes(env: &Env) -> usize {
    env.var("MAX_DOCUMENT_BYTES")
        .ok()
        .and_then(|v| v.to_string().parse().ok())
        .unwrap_or(DEFAULT_MAX_DOCUMENT_BYTES)
}

pub async fn handle_upload(
    kv: KvStore,
    env: &Env,
    workflow: &str,
    role: &str,
    mut req: Request,
) -> Result<Response> {
    if let Err(resp) = validate_role_name(role) {
        return Ok(resp);
    }

    let body = req.text().await.unwrap_or_default();

    if body.is_empty() {
        return json_error(400, "Request body must not be empty", "bad_request", None);
    }

    let byte_len = body.len();
    let max_bytes = max_document_bytes(env);
    if byte_len > max_bytes {
        return json_error(
            400,
            &format!("Document exceeds maximum size ({byte_len} bytes > {max_bytes} bytes)"),
            "bad_request",
            None,
        );
    }

    let key = doc_key(workflow, role);
    kv_put_text(&kv, &key, &body).await?;

    let mut warnings = Vec::new();
    if byte_len < SMALL_DOCUMENT_THRESHOLD {
        warnings.push(format!(
            "Document is unusually small ({byte_len} bytes). Verify this is the correct content."
        ));
    }

    success_response(
        json!({
            "workflow": workflow,
            "role": role,
            "key": key,
            "bytes": byte_len,
        }),
        warnings,
        None,
    )
}

pub async fn handle_get(kv: KvStore, workflow: &str, role: &str) -> Result<Response> {
    if let Err(resp) = validate_role_name(role) {
        return Ok(resp);
    }

    let key = doc_key(workflow, role);
    match kv_get_text(&kv, &key).await? {
        Some(content) => {
            let headers = Headers::new();
            headers.set("Content-Type", "text/markdown")?;
            Ok(Response::ok(content)?.with_headers(headers))
        }
        None => json_error(
            404,
            &format!("Document '{role}' does not exist for workflow '{workflow}'"),
            "not_found",
            Some(&format!(
                "Upload this document with PUT /documents/{workflow}/{role}"
            )),
        ),
    }
}
