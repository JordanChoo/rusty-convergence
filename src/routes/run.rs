use chrono::DateTime;
use serde_json::json;
use worker::kv::KvStore;
use worker::*;

use crate::convergence::{update_meta_after_round, update_stats_after_round};
use crate::error::{json_error, now_iso8601, success_response};
use crate::metrics::compute_metrics;
use crate::prompt::{render_template, select_template, RenderError};
use crate::providers::parse_sse_events;
use crate::providers::StreamChunk;
use crate::storage::{
    acquire_lock, config_key, kv_get, kv_put, release_lock, round_key,
};
use crate::types::{Round, RoundStatus, RunOverrides, UsageStats, Workflow};
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

pub struct RoundCompletionSummary {
    pub words: u32,
    pub lines: u32,
    pub characters: u32,
    pub headings: u32,
    pub convergence_score: Option<f64>,
    pub recommendation: Option<String>,
    pub duration_seconds: u64,
}

pub async fn on_round_complete(
    kv: &KvStore,
    workflow: &str,
    round_num: u32,
    content: &str,
    usage: Option<crate::types::UsageStats>,
    provider: &str,
    model: &str,
    include_impl: bool,
    started_at: &str,
    _warnings: Vec<String>,
) -> Result<RoundCompletionSummary> {
    let metrics = compute_metrics(content);

    let convergence =
        update_stats_after_round(kv, workflow, round_num, content, metrics.words).await?;

    let now = now_iso8601();
    let duration = compute_duration(started_at, &now);

    let complete_round = Round {
        workflow: workflow.to_string(),
        round: round_num,
        status: RoundStatus::Complete,
        content: Some(content.to_string()),
        partial_content: None,
        metrics: Some(metrics.clone()),
        convergence: Some(convergence.clone()),
        usage,
        provider: provider.to_string(),
        model: model.to_string(),
        include_impl,
        started_at: started_at.to_string(),
        completed_at: Some(now),
        failed_at: None,
        duration_seconds: Some(duration),
        error: None,
    };

    kv_put(kv, &round_key(workflow, round_num), &complete_round).await?;

    update_meta_after_round(kv, workflow, round_num, convergence.score).await?;

    release_lock(kv, workflow).await?;

    Ok(RoundCompletionSummary {
        words: metrics.words,
        lines: metrics.lines,
        characters: metrics.characters,
        headings: metrics.headings,
        convergence_score: convergence.score,
        recommendation: convergence.recommendation,
        duration_seconds: duration,
    })
}

pub async fn on_round_failed(
    kv: &KvStore,
    workflow: &str,
    round_num: u32,
    error_msg: &str,
    partial_content: Option<&str>,
    provider: &str,
    model: &str,
    include_impl: bool,
    started_at: &str,
) -> Result<()> {
    let now = now_iso8601();
    let failed_round = Round {
        workflow: workflow.to_string(),
        round: round_num,
        status: RoundStatus::Failed,
        content: None,
        partial_content: partial_content.map(|s| s.to_string()),
        metrics: None,
        convergence: None,
        usage: None,
        provider: provider.to_string(),
        model: model.to_string(),
        include_impl,
        started_at: started_at.to_string(),
        completed_at: None,
        failed_at: Some(now),
        duration_seconds: None,
        error: Some(error_msg.to_string()),
    };

    kv_put(kv, &round_key(workflow, round_num), &failed_round).await?;

    release_lock(kv, workflow).await?;

    Ok(())
}

fn compute_duration(started_at: &str, completed_at: &str) -> u64 {
    let Ok(start) = chrono::DateTime::parse_from_rfc3339(started_at) else {
        return 0;
    };
    let Ok(end) = chrono::DateTime::parse_from_rfc3339(completed_at) else {
        return 0;
    };
    let diff = end.timestamp() - start.timestamp();
    if diff > 0 { diff as u64 } else { 0 }
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
    let api_key = match env.secret(api_key_name) {
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

    let provider_params = overrides
        .provider_params
        .as_ref()
        .or(wf.provider_params.as_ref());

    let (api_url, request_body, auth_headers) = match provider.as_str() {
        "openai" => {
            let body = crate::providers::openai::build_request_body(
                &model,
                system_prompt.as_deref(),
                &rendered_prompt,
                provider_params,
            );
            let headers = Headers::new();
            headers.set("Authorization", &format!("Bearer {api_key}"))?;
            headers.set("Content-Type", "application/json")?;
            (crate::providers::openai::API_URL, body, headers)
        }
        "anthropic" => {
            let body = crate::providers::anthropic::build_request_body(
                &model,
                system_prompt.as_deref(),
                &rendered_prompt,
                provider_params,
            );
            let headers = Headers::new();
            headers.set("x-api-key", &api_key)?;
            headers.set("anthropic-version", crate::providers::anthropic::API_VERSION)?;
            headers.set("Content-Type", "application/json")?;
            (crate::providers::anthropic::API_URL, body, headers)
        }
        _ => unreachable!(),
    };

    let mut fetch_init = RequestInit::new();
    fetch_init.with_method(Method::Post);
    fetch_init.with_headers(auth_headers);
    fetch_init.with_body(Some(wasm_bindgen::JsValue::from_str(
        &request_body.to_string(),
    )));

    let fetch_request = Request::new_with_init(api_url, &fetch_init)?;
    let mut llm_response = Fetch::Request(fetch_request).send().await?;

    let status_code = llm_response.status_code();
    if status_code != 200 {
        let error_body = llm_response.text().await.unwrap_or_default();
        let error_msg = format!("provider_error: HTTP {status_code}: {error_body}");
        on_round_failed(
            &kv,
            workflow,
            round,
            &error_msg,
            None,
            &provider,
            &model,
            include_impl,
            &now,
        )
        .await?;
        return json_error(502, &error_msg, "provider_error", None);
    }

    let raw_body = match llm_response.text().await {
        Ok(b) => b,
        Err(e) => {
            let error_msg = format!("Failed to read provider response: {e}");
            on_round_failed(
                &kv,
                workflow,
                round,
                &error_msg,
                None,
                &provider,
                &model,
                include_impl,
                &now,
            )
            .await?;
            return json_error(502, &error_msg, "provider_error", None);
        }
    };

    let events = parse_sse_events(&raw_body);
    let mut content_buffer = String::new();
    let mut usage: Option<UsageStats> = None;

    match provider.as_str() {
        "openai" => {
            for event in &events {
                match crate::providers::openai::parse_event(event) {
                    Ok(StreamChunk::Text(t)) => content_buffer.push_str(&t),
                    Ok(StreamChunk::Usage(u)) => usage = Some(u),
                    Ok(StreamChunk::Done) => break,
                    Err(e) => {
                        let partial = if content_buffer.is_empty() {
                            None
                        } else {
                            Some(content_buffer.as_str())
                        };
                        on_round_failed(
                            &kv,
                            workflow,
                            round,
                            &e.to_string(),
                            partial,
                            &provider,
                            &model,
                            include_impl,
                            &now,
                        )
                        .await?;
                        return json_error(502, &e.to_string(), "provider_error", None);
                    }
                }
            }
        }
        "anthropic" => {
            let mut state = crate::providers::anthropic::AnthropicStreamState::default();
            for event in &events {
                match crate::providers::anthropic::parse_event(event, &mut state) {
                    Ok(StreamChunk::Text(t)) => content_buffer.push_str(&t),
                    Ok(StreamChunk::Usage(u)) => usage = Some(u),
                    Ok(StreamChunk::Done) => {
                        usage = Some(crate::providers::anthropic::finalize_usage(&state));
                        break;
                    }
                    Err(e) => {
                        let partial = if content_buffer.is_empty() {
                            None
                        } else {
                            Some(content_buffer.as_str())
                        };
                        on_round_failed(
                            &kv,
                            workflow,
                            round,
                            &e.to_string(),
                            partial,
                            &provider,
                            &model,
                            include_impl,
                            &now,
                        )
                        .await?;
                        return json_error(502, &e.to_string(), "provider_error", None);
                    }
                }
            }
        }
        _ => unreachable!(),
    }

    if content_buffer.is_empty() {
        on_round_failed(
            &kv,
            workflow,
            round,
            "Empty response from provider",
            None,
            &provider,
            &model,
            include_impl,
            &now,
        )
        .await?;
        return json_error(502, "Empty response from provider", "provider_error", None);
    }

    let summary = on_round_complete(
        &kv,
        workflow,
        round,
        &content_buffer,
        usage.clone(),
        &provider,
        &model,
        include_impl,
        &now,
        template_warnings,
    )
    .await?;

    success_response(
        json!({
            "workflow": workflow,
            "round": round,
            "status": "complete",
            "content": content_buffer,
            "metrics": {
                "words": summary.words,
                "lines": summary.lines,
                "characters": summary.characters,
                "headings": summary.headings,
            },
            "convergence": {
                "score": summary.convergence_score,
                "estimated_remaining_rounds": summary.recommendation.as_deref().map(|r| match r {
                    "stop" => "0",
                    "almost" => "1-2",
                    "continue" => "3-5",
                    _ => "5+",
                }),
                "recommendation": summary.recommendation,
            },
            "usage": usage,
            "provider": provider,
            "model": model,
            "started_at": now,
            "completed_at": now_iso8601(),
            "duration_seconds": summary.duration_seconds,
        }),
        vec![],
        None,
    )
}
