use chrono::DateTime;
use serde_json::json;
use worker::kv::KvStore;
use worker::*;

use crate::convergence::{update_meta_after_round, update_stats_after_round};
use crate::error::{json_error, now_iso8601, success_response};
use crate::metrics::compute_metrics;
use crate::prompt::{extract_placeholders, render_template, select_template, RenderError};
use crate::storage::{
    acquire_lock, check_lock, config_key, kv_get, kv_put, release_lock, round_key,
};
use crate::types::{Round, RoundStatus, RunOverrides, Workflow};
use crate::validation::parse_and_validate_round;

#[derive(Debug)]
pub enum RoundAction {
    Proceed,
    Retry,
    Conflict(String),
}

pub fn default_lock_ttl(env: &Env) -> u64 {
    env.var("DEFAULT_LOCK_TTL_SECONDS")
        .ok()
        .and_then(|v| v.to_string().parse().ok())
        .unwrap_or(3600)
}

pub async fn check_previous_round(
    kv: &KvStore,
    workflow: &str,
    round: u32,
    skip: bool,
) -> Result<Response> {
    if round <= 1 || skip {
        return json_error(0, "", "", None); // sentinel: won't be used
    }
    let prev_key = round_key(workflow, round - 1);
    match kv_get::<Round>(kv, &prev_key).await? {
        Some(prev) if prev.status == RoundStatus::Complete => {
            json_error(0, "", "", None) // sentinel
        }
        Some(_) => json_error(
            422,
            &format!(
                "Round {} must be completed before running round {round}",
                round - 1
            ),
            "validation_failed",
            None,
        ),
        None => json_error(
            422,
            &format!(
                "Round {} must be completed before running round {round}",
                round - 1
            ),
            "validation_failed",
            None,
        ),
    }
}

pub async fn check_round_status(
    kv: &KvStore,
    workflow: &str,
    round: u32,
    lock_ttl_seconds: u64,
) -> Result<RoundAction> {
    let key = round_key(workflow, round);
    match kv_get::<Round>(kv, &key).await? {
        None => Ok(RoundAction::Proceed),
        Some(r) => match r.status {
            RoundStatus::Complete => Ok(RoundAction::Conflict(
                "Round already completed; cannot overwrite".to_string(),
            )),
            RoundStatus::Failed | RoundStatus::Stale => Ok(RoundAction::Retry),
            RoundStatus::Running => {
                if is_stale(&r.started_at, lock_ttl_seconds) {
                    Ok(RoundAction::Retry)
                } else {
                    Ok(RoundAction::Conflict(format!(
                        "Another run is in progress (started at {})",
                        r.started_at
                    )))
                }
            }
        },
    }
}

pub fn is_stale(started_at: &str, lock_ttl_seconds: u64) -> bool {
    let Ok(start) = DateTime::parse_from_rfc3339(started_at) else {
        return true;
    };
    let now_millis = Date::now().as_millis();
    let now_secs = (now_millis / 1000) as i64;
    let elapsed = now_secs - start.timestamp();
    elapsed > lock_ttl_seconds as i64
}

pub fn effective_status(round: &Round, lock_ttl_seconds: u64) -> RoundStatus {
    if round.status == RoundStatus::Running && is_stale(&round.started_at, lock_ttl_seconds) {
        RoundStatus::Stale
    } else {
        round.status.clone()
    }
}

pub async fn handle(
    kv: KvStore,
    env: &Env,
    workflow: &str,
    round_str: &str,
    mut req: Request,
) -> Result<Response> {
    let round = match parse_and_validate_round(round_str) {
        Ok(n) => n,
        Err(resp) => return Ok(resp),
    };

    let config_k = config_key(workflow);
    let wf = match kv_get::<Workflow>(&kv, &config_k).await? {
        Some(w) => w,
        None => {
            return json_error(
                404,
                &format!("Workflow '{workflow}' does not exist"),
                "not_found",
                Some("Use POST /workflows to create a workflow first"),
            )
        }
    };

    let overrides: RunOverrides = match req.text().await {
        Ok(body) if !body.is_empty() => serde_json::from_str(&body).unwrap_or(RunOverrides {
            include_impl: None,
            skip_sequence_check: None,
            provider: None,
            model: None,
            system_prompt: None,
            provider_params: None,
        }),
        _ => RunOverrides {
            include_impl: None,
            skip_sequence_check: None,
            provider: None,
            model: None,
            system_prompt: None,
            provider_params: None,
        },
    };

    let skip_seq = overrides.skip_sequence_check.unwrap_or(false);
    if round > 1 && !skip_seq {
        let prev_key = round_key(workflow, round - 1);
        match kv_get::<Round>(&kv, &prev_key).await? {
            Some(prev) if prev.status == RoundStatus::Complete => {}
            _ => {
                return json_error(
                    422,
                    &format!(
                        "Round {} must be completed before running round {round}",
                        round - 1
                    ),
                    "validation_failed",
                    None,
                )
            }
        }
    }

    let lock_ttl = default_lock_ttl(env);
    match check_round_status(&kv, workflow, round, lock_ttl).await? {
        RoundAction::Proceed | RoundAction::Retry => {}
        RoundAction::Conflict(msg) => return json_error(409, &msg, "conflict", None),
    }

    let provider = overrides
        .provider
        .as_deref()
        .unwrap_or(&wf.provider)
        .to_string();
    let model = overrides
        .model
        .as_deref()
        .unwrap_or(&wf.model)
        .to_string();
    let system_prompt = overrides
        .system_prompt
        .as_ref()
        .or(wf.system_prompt.as_ref())
        .cloned();

    let api_key_name = match provider.as_str() {
        "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        _ => {
            return json_error(
                400,
                &format!("Unknown provider '{provider}'. Must be 'openai' or 'anthropic'"),
                "bad_request",
                None,
            )
        }
    };
    let _api_key = match env.secret(api_key_name) {
        Ok(s) => s.to_string(),
        Err(_) => {
            return json_error(
                500,
                &format!("Missing secret: {api_key_name}"),
                "missing_config",
                None,
            )
        }
    };

    let (template, include_impl) = select_template(&wf, round, overrides.include_impl);
    let documents = if wf.documents.is_empty() {
        crate::prompt::default_documents_map(include_impl)
    } else {
        wf.documents.clone()
    };

    let (rendered_prompt, template_warnings) =
        match render_template(template, workflow, &documents, &kv).await {
            Ok(r) => r,
            Err(RenderError::MissingRole(role)) => {
                return json_error(
                    422,
                    &format!("Template references role '{role}' which is not in the documents map"),
                    "validation_failed",
                    None,
                )
            }
            Err(RenderError::MissingDocument(role, doc_role)) => {
                return json_error(
                    422,
                    &format!(
                        "Document for role '{role}' (key: doc::{workflow}::{doc_role}) not found in KV"
                    ),
                    "validation_failed",
                    Some(&format!(
                        "Upload it with PUT /documents/{workflow}/{doc_role}"
                    )),
                )
            }
            Err(RenderError::KvError(e)) => {
                return json_error(500, &format!("KV error: {e}"), "internal_error", None)
            }
        };

    if let Err(existing) = acquire_lock(&kv, workflow, round, lock_ttl).await? {
        return json_error(
            409,
            &format!(
                "Workflow '{}' is locked by round {} (started at {})",
                workflow, existing.round, existing.started_at
            ),
            "conflict",
            Some("Use GET /rounds to check status, or wait for the lock to expire"),
        );
    }

    let now = now_iso8601();
    let running_round = Round {
        workflow: workflow.to_string(),
        round,
        status: RoundStatus::Running,
        content: None,
        partial_content: None,
        metrics: None,
        convergence: None,
        usage: None,
        provider: provider.clone(),
        model: model.clone(),
        include_impl,
        started_at: now.clone(),
        completed_at: None,
        failed_at: None,
        duration_seconds: None,
        error: None,
    };
    kv_put(&kv, &round_key(workflow, round), &running_round).await?;

    // TODO: Call LLM provider and stream response back to client.
    // For now, return a structured response indicating the run was started
    // but LLM integration is pending.
    release_lock(&kv, workflow).await?;

    let failed_round = Round {
        status: RoundStatus::Failed,
        error: Some("LLM provider integration not yet implemented".to_string()),
        failed_at: Some(now_iso8601()),
        ..running_round
    };
    kv_put(&kv, &round_key(workflow, round), &failed_round).await?;

    json_error(
        501,
        "LLM provider streaming not yet implemented. Run was validated and lock was acquired/released successfully.",
        "not_implemented",
        Some("The pre-run validation, lock management, template rendering, and round lifecycle are functional. Awaiting provider adapter implementation."),
    )
}
