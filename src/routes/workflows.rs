use serde_json::json;
use worker::kv::KvStore;
use worker::*;

use crate::error::{json_error, now_iso8601, success_response};
use crate::prompt::{
    default_documents_map, extract_placeholders, DEFAULT_TEMPLATE, DEFAULT_TEMPLATE_WITH_IMPL,
};
use crate::storage::{
    config_key, doc_key, kv_delete, kv_get, kv_get_text, kv_list_by_prefix, kv_put, lock_key,
    meta_key, stats_key,
};
use crate::types::{Meta, Workflow};
use crate::validation::{check_role_name, validate_workflow_name};

const SYNTHETIC_PLACEHOLDERS: &[&str] = &["previous_round"];

pub async fn handle_create(kv: KvStore, mut req: Request) -> Result<Response> {
    let body: serde_json::Value = match req.json().await {
        Ok(v) => v,
        Err(_) => {
            return json_error(
                400,
                "Invalid or missing JSON body",
                "bad_request",
                Some("Request body must be valid JSON with Content-Type: application/json"),
            );
        }
    };

    let name = match body["name"].as_str() {
        Some(n) => n,
        None => {
            return json_error(
                400,
                "Missing or non-string required field: name",
                "bad_request",
                None,
            );
        }
    };

    if let Err(resp) = validate_workflow_name(name) {
        return Ok(resp);
    }

    let provider = match body["provider"].as_str() {
        Some(p) if p == "openai" || p == "anthropic" => p.to_string(),
        Some(p) => {
            return json_error(
                400,
                &format!("Invalid provider '{p}'. Must be 'openai' or 'anthropic'"),
                "bad_request",
                None,
            );
        }
        None => {
            return json_error(400, "Missing required field: provider", "bad_request", None);
        }
    };

    let model = match body["model"].as_str() {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => {
            return json_error(
                400,
                "Missing or empty required field: model",
                "bad_request",
                None,
            );
        }
    };

    let template = body["template"].as_str().map(String::from);
    let template_with_impl = body["template_with_impl"].as_str().map(String::from);
    let impl_every_n = body["impl_every_n"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok());

    let validate_impl_template =
        implementation_template_is_active(template_with_impl.as_deref(), impl_every_n);
    let effective_template = template.as_deref().unwrap_or(DEFAULT_TEMPLATE);
    let effective_impl_template = template_with_impl
        .as_deref()
        .or(template.as_deref())
        .unwrap_or(DEFAULT_TEMPLATE_WITH_IMPL);

    let documents: std::collections::HashMap<String, String> = match body.get("documents") {
        Some(serde_json::Value::Object(map)) => map
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        Some(_) => {
            return json_error(
                400,
                "Field 'documents' must be an object",
                "bad_request",
                None,
            );
        }
        None => {
            let has_impl_placeholder = validate_impl_template
                && extract_placeholders(effective_impl_template)
                    .iter()
                    .any(|p| p == "implementation");
            default_documents_map(has_impl_placeholder)
        }
    };

    for (role_name, role_id) in &documents {
        if let Err(e) = check_role_name(role_name) {
            return json_error(
                400,
                &format!("Invalid document role name '{}': {}", role_name, e.message),
                "bad_request",
                None,
            );
        }
        if let Err(e) = check_role_name(role_id) {
            return json_error(
                400,
                &format!(
                    "Invalid document role identifier '{}' for role '{}': {}",
                    role_id, role_name, e.message
                ),
                "bad_request",
                Some(
                    "Role identifiers must be alphanumeric, hyphens, or underscores (max 32 chars)",
                ),
            );
        }
    }

    let template_placeholders = extract_placeholders(effective_template);
    for placeholder in &template_placeholders {
        if SYNTHETIC_PLACEHOLDERS.contains(&placeholder.as_str()) {
            continue;
        }
        if !documents.contains_key(placeholder.as_str()) {
            return json_error(
                422,
                &format!(
                    "Template references role '{}' but it is not defined in the documents map",
                    placeholder
                ),
                "validation_failed",
                None,
            );
        }
    }

    if validate_impl_template {
        let impl_placeholders = extract_placeholders(effective_impl_template);
        for placeholder in &impl_placeholders {
            if SYNTHETIC_PLACEHOLDERS.contains(&placeholder.as_str()) {
                continue;
            }
            if !documents.contains_key(placeholder.as_str()) {
                return json_error(
                    422,
                    &format!(
                        "Implementation template references role '{}' but it is not defined in the documents map",
                        placeholder
                    ),
                    "validation_failed",
                    None,
                );
            }
        }
    }

    let mut missing_docs = Vec::new();
    for (role_name, role_id) in &documents {
        let key = doc_key(name, role_id);
        if kv_get_text(&kv, &key).await?.is_none() {
            missing_docs.push(role_name.clone());
        }
    }
    if !missing_docs.is_empty() {
        missing_docs.sort();
        return json_error(
            422,
            &format!(
                "Documents not found in KV for roles: {}. Upload them with PUT /documents/{}/{{role}} first.",
                missing_docs.join(", "),
                name
            ),
            "validation_failed",
            Some(&format!(
                "Upload missing documents with PUT /documents/{}/{{role}}",
                name
            )),
        );
    }

    let mut all_referenced: Vec<String> = template_placeholders;
    if validate_impl_template {
        let impl_placeholders = extract_placeholders(effective_impl_template);
        for p in impl_placeholders {
            if !all_referenced.contains(&p) {
                all_referenced.push(p);
            }
        }
    }

    let mut warnings = Vec::new();
    for role_name in documents.keys() {
        if !all_referenced.contains(role_name) {
            warnings.push(format!(
                "Document role '{}' is configured but not referenced by any template",
                role_name
            ));
        }
    }

    let workflow = Workflow {
        name: name.to_string(),
        description: body["description"].as_str().map(String::from),
        provider,
        model,
        system_prompt: body["system_prompt"].as_str().map(String::from),
        provider_params: body.get("provider_params").cloned(),
        documents,
        template,
        template_with_impl,
        impl_every_n,
    };

    let key = config_key(name);
    kv_put(&kv, &key, &workflow).await?;

    success_response(json!(workflow), warnings, None)
}

fn implementation_template_is_active(
    template_with_impl: Option<&str>,
    impl_every_n: Option<u32>,
) -> bool {
    template_with_impl.is_some() || impl_every_n.is_some_and(|n| n > 0)
}

pub async fn handle_list(kv: KvStore, url: &Url) -> Result<Response> {
    let params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v.min(100))
        .unwrap_or(100);

    let cursor = params.get("cursor").map(|s| s.as_str());
    let prefix = "config::";

    let (keys, next_cursor) = kv_list_by_prefix(&kv, prefix, limit, cursor).await?;

    let mut workflows = Vec::new();
    for key in &keys {
        let wf_name = key.strip_prefix(prefix).unwrap_or(key);
        if let Some(wf) = kv_get::<Workflow>(&kv, key).await? {
            let meta: Option<Meta> = kv_get(&kv, &meta_key(wf_name)).await?;

            workflows.push(json!({
                "name": wf.name,
                "description": wf.description,
                "provider": wf.provider,
                "model": wf.model,
                "round_count": meta.as_ref().map_or(0, |m| m.round_count),
                "latest_round": meta.as_ref().and_then(|m| m.latest_round),
                "convergence_score": meta.as_ref().and_then(|m| m.latest_convergence),
            }));
        }
    }

    success_response(
        json!({
            "workflows": workflows,
            "cursor": next_cursor,
        }),
        vec![],
        None,
    )
}

pub async fn handle_get(kv: KvStore, name: &str) -> Result<Response> {
    if let Err(resp) = validate_workflow_name(name) {
        return Ok(resp);
    }

    let key = config_key(name);
    let wf: Workflow = match kv_get(&kv, &key).await? {
        Some(w) => w,
        None => {
            return json_error(
                404,
                &format!("Workflow '{name}' does not exist"),
                "not_found",
                Some("Use POST /workflows to create a workflow first"),
            );
        }
    };

    let meta: Option<Meta> = kv_get(&kv, &meta_key(name)).await?;

    let now = now_iso8601();
    let created_at = meta
        .as_ref()
        .map_or_else(|| now.clone(), |m| m.created_at.clone());
    let updated_at = meta
        .as_ref()
        .map_or_else(|| now.clone(), |m| m.updated_at.clone());

    let mut data = serde_json::to_value(&wf).unwrap_or(json!({}));
    if let Some(obj) = data.as_object_mut() {
        obj.insert(
            "round_count".into(),
            json!(meta.as_ref().map_or(0, |m| m.round_count)),
        );
        obj.insert(
            "latest_round".into(),
            json!(meta.as_ref().and_then(|m| m.latest_round)),
        );
        obj.insert(
            "convergence_score".into(),
            json!(meta.as_ref().and_then(|m| m.latest_convergence)),
        );
        obj.insert("created_at".into(), json!(created_at));
        obj.insert("updated_at".into(), json!(updated_at));
    }

    success_response(data, vec![], None)
}

pub async fn handle_delete(kv: KvStore, name: &str) -> Result<Response> {
    if let Err(resp) = validate_workflow_name(name) {
        return Ok(resp);
    }

    let key = config_key(name);
    if kv_get::<Workflow>(&kv, &key).await?.is_none() {
        return json_error(
            404,
            &format!("Workflow '{name}' does not exist"),
            "not_found",
            None,
        );
    }

    kv_delete(&kv, &key).await?;
    let mut keys_removed: u32 = 1;

    for prefix in &[format!("round::{}::", name), format!("doc::{}::", name)] {
        let mut cursor: Option<String> = None;
        loop {
            let (keys, next) = kv_list_by_prefix(&kv, prefix, 100, cursor.as_deref()).await?;
            for k in &keys {
                if kv_delete(&kv, k).await.is_ok() {
                    keys_removed += 1;
                }
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
    }

    for extra_key in &[meta_key(name), stats_key(name), lock_key(name)] {
        if kv_delete(&kv, extra_key).await.is_ok() {
            keys_removed += 1;
        }
    }

    success_response(
        json!({
            "deleted": name,
            "keys_removed": keys_removed,
        }),
        vec![],
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::implementation_template_is_active;

    #[test]
    fn implementation_template_inactive_for_normal_workflow() {
        assert!(!implementation_template_is_active(None, None));
    }

    #[test]
    fn implementation_template_inactive_for_zero_interval() {
        assert!(!implementation_template_is_active(None, Some(0)));
    }

    #[test]
    fn implementation_template_active_when_explicit_template_present() {
        assert!(implementation_template_is_active(
            Some("{{implementation}}"),
            None
        ));
    }

    #[test]
    fn implementation_template_active_when_interval_enabled() {
        assert!(implementation_template_is_active(None, Some(4)));
    }
}
