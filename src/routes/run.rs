use chrono::DateTime;
use serde_json::json;
use worker::kv::KvStore;
use worker::*;

use crate::convergence::{commit_stats, compute_stats_update, update_meta_after_round};
use crate::error::{json_error, now_iso8601, success_response};
use crate::metrics::compute_metrics;
use crate::prompt::{render_template, select_template, RenderError};
use crate::providers::parse_sse_events;
use crate::providers::StreamChunk;
use crate::storage::{acquire_lock, config_key, kv_get, kv_put, release_lock, round_key};
use crate::types::{
    ConvergenceData, DocumentMetrics, Round, RoundStatus, RunOverrides, UsageStats, Workflow,
};
use crate::validation::{parse_and_validate_round, validate_workflow_name};

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
        .min(86400) // cap at 24 hours to prevent i64 overflow in chrono::Duration
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
    pub convergence: crate::types::ConvergenceData,
    pub completed_at: String,
    pub duration_seconds: u64,
}

#[derive(Debug)]
pub struct RoundResult {
    pub content: String,
    pub metrics: DocumentMetrics,
    pub convergence: ConvergenceData,
    pub usage: Option<UsageStats>,
    pub provider: String,
    pub model: String,
    pub provider_params: Option<serde_json::Value>,
    pub include_impl: bool,
    pub started_at: String,
    pub completed_at: String,
    pub duration_seconds: u64,
    pub template_warnings: Vec<String>,
}

#[derive(Debug)]
pub enum ExecutionError {
    Conflict(String, Option<String>),
    BadRequest(String),
    Validation(String, Option<String>),
    Provider(String, Option<String>),
    MissingConfig(String),
    Internal(String),
}

impl ExecutionError {
    pub fn is_round_already_complete(&self) -> bool {
        matches!(self, Self::Conflict(msg, _) if msg.contains("already completed"))
    }

    pub fn into_response(self) -> Result<Response> {
        match self {
            Self::Conflict(msg, hint) => json_error(409, &msg, "conflict", hint.as_deref()),
            Self::BadRequest(msg) => json_error(400, &msg, "bad_request", None),
            Self::Validation(msg, hint) => {
                json_error(422, &msg, "validation_failed", hint.as_deref())
            }
            Self::Provider(msg, hint) => json_error(502, &msg, "provider_error", hint.as_deref()),
            Self::MissingConfig(msg) => json_error(500, &msg, "missing_config", None),
            Self::Internal(msg) => json_error(500, &msg, "internal_error", None),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn on_round_complete(
    kv: &KvStore,
    workflow: &str,
    round_num: u32,
    content: &str,
    usage: Option<crate::types::UsageStats>,
    provider: &str,
    model: &str,
    provider_params: Option<serde_json::Value>,
    include_impl: bool,
    started_at: &str,
) -> Result<RoundCompletionSummary> {
    let metrics = compute_metrics(content);

    let computed = compute_stats_update(kv, workflow, round_num, content, metrics.words).await?;

    let completed_at = now_iso8601();
    let duration = compute_duration(started_at, &completed_at);

    // Write round record FIRST — this is the most critical write.
    // If it fails, stats/meta are not yet updated, so no phantom entries.
    let complete_round = Round {
        workflow: workflow.to_string(),
        round: round_num,
        status: RoundStatus::Complete,
        content: Some(content.to_string()),
        partial_content: None,
        metrics: Some(metrics.clone()),
        convergence: Some(computed.convergence.clone()),
        usage,
        provider: provider.to_string(),
        model: model.to_string(),
        provider_params,
        include_impl,
        started_at: started_at.to_string(),
        completed_at: Some(completed_at.clone()),
        failed_at: None,
        duration_seconds: Some(duration),
        error: None,
    };

    kv_put(kv, &round_key(workflow, round_num), &complete_round).await?;

    // Stats and meta are recoverable via POST /stats/rebuild if these fail.
    // Use let _ = so that a stats/meta write failure doesn't prevent the
    // response from being sent — the round record is already saved.
    if let Err(e) = commit_stats(kv, workflow, &computed.updated_stats).await {
        console_log!("warn: commit_stats failed for {workflow} round {round_num}: {e}");
    }
    if let Err(e) =
        update_meta_after_round(kv, workflow, round_num, computed.convergence.score).await
    {
        console_log!("warn: update_meta failed for {workflow} round {round_num}: {e}");
    }

    let _ = release_lock(kv, workflow).await;

    Ok(RoundCompletionSummary {
        words: metrics.words,
        lines: metrics.lines,
        characters: metrics.characters,
        headings: metrics.headings,
        convergence: computed.convergence,
        completed_at,
        duration_seconds: duration,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn on_round_failed(
    kv: &KvStore,
    workflow: &str,
    round_num: u32,
    error_msg: &str,
    partial_content: Option<&str>,
    provider: &str,
    model: &str,
    provider_params: Option<serde_json::Value>,
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
        provider_params,
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
    if diff > 0 {
        diff as u64
    } else {
        0
    }
}

pub async fn execute_round(
    kv: &KvStore,
    env: &Env,
    wf: &Workflow,
    workflow_name: &str,
    round: u32,
    overrides: &RunOverrides,
) -> std::result::Result<RoundResult, ExecutionError> {
    let lock_ttl = default_lock_ttl(env);
    match check_round_status(kv, workflow_name, round, lock_ttl).await {
        Ok(RoundAction::Proceed | RoundAction::Retry) => {}
        Ok(RoundAction::Conflict(msg)) => return Err(ExecutionError::Conflict(msg, None)),
        Err(e) => return Err(ExecutionError::Internal(e.to_string())),
    }

    match acquire_lock(kv, workflow_name, round, lock_ttl).await {
        Ok(Ok(())) => {}
        Ok(Err(existing)) => {
            return Err(ExecutionError::Conflict(
                format!(
                    "Workflow '{}' is locked by round {} (started at {})",
                    workflow_name, existing.round, existing.started_at
                ),
                Some("Use GET /rounds to check status, or wait for the lock to expire".to_string()),
            ));
        }
        Err(e) => return Err(ExecutionError::Internal(e.to_string())),
    }

    let provider = overrides
        .provider
        .as_deref()
        .unwrap_or(&wf.provider)
        .to_string();
    let model = overrides.model.as_deref().unwrap_or(&wf.model).to_string();
    let system_prompt = overrides
        .system_prompt
        .as_ref()
        .or(wf.system_prompt.as_ref())
        .cloned();

    let api_key_name = match provider.as_str() {
        "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        _ => {
            let _ = release_lock(kv, workflow_name).await;
            return Err(ExecutionError::BadRequest(format!(
                "Unknown provider '{provider}'. Must be 'openai' or 'anthropic'"
            )));
        }
    };
    let api_key = match env.secret(api_key_name) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let _ = release_lock(kv, workflow_name).await;
            return Err(ExecutionError::MissingConfig(format!(
                "Missing secret: {api_key_name}"
            )));
        }
    };

    let (template, include_impl) = select_template(wf, round, overrides.include_impl);
    let documents = if wf.documents.is_empty() {
        crate::prompt::default_documents_map(include_impl)
    } else {
        wf.documents.clone()
    };

    let effective_template = if template.contains("{{previous_round}}") {
        if round > 1 {
            let prev = match kv_get::<Round>(kv, &round_key(workflow_name, round - 1)).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = release_lock(kv, workflow_name).await;
                    return Err(ExecutionError::Internal(e.to_string()));
                }
            };
            let prev_content = prev
                .and_then(|r| r.content)
                .unwrap_or_else(|| "(Prior round has no content.)".to_string());
            template.replace("{{previous_round}}", &prev_content)
        } else {
            template.replace(
                "{{previous_round}}",
                "(This is the first review round — no prior analysis available.)",
            )
        }
    } else {
        template.to_string()
    };

    let (rendered_prompt, template_warnings) = match render_template(
        &effective_template,
        workflow_name,
        &documents,
        kv,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = release_lock(kv, workflow_name).await;
            return match e {
                RenderError::MissingRole(role) => Err(ExecutionError::Validation(
                    format!(
                        "Template references role '{role}' which is not in the documents map"
                    ),
                    None,
                )),
                RenderError::MissingDocument(role, doc_role) => Err(ExecutionError::Validation(
                    format!(
                        "Document for role '{role}' (key: doc::{workflow_name}::{doc_role}) not found in KV"
                    ),
                    Some(format!(
                        "Upload it with PUT /documents/{workflow_name}/{doc_role}"
                    )),
                )),
                RenderError::KvError(e) => {
                    Err(ExecutionError::Internal(format!("KV error: {e}")))
                }
            };
        }
    };

    let provider_params = overrides
        .provider_params
        .clone()
        .or_else(|| wf.provider_params.clone());

    let now = now_iso8601();
    let running_round = Round {
        workflow: workflow_name.to_string(),
        round,
        status: RoundStatus::Running,
        content: None,
        partial_content: None,
        metrics: None,
        convergence: None,
        usage: None,
        provider: provider.clone(),
        model: model.clone(),
        provider_params: provider_params.clone(),
        include_impl,
        started_at: now.clone(),
        completed_at: None,
        failed_at: None,
        duration_seconds: None,
        error: None,
    };
    if let Err(e) = kv_put(kv, &round_key(workflow_name, round), &running_round).await {
        let _ = release_lock(kv, workflow_name).await;
        return Err(ExecutionError::Internal(e.to_string()));
    }

    let build_fetch_result: Result<(Response,)> = async {
        let (api_url, request_body, auth_headers) = match provider.as_str() {
            "openai" => {
                let body = crate::providers::openai::build_request_body(
                    &model,
                    system_prompt.as_deref(),
                    &rendered_prompt,
                    provider_params.as_ref(),
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
                    provider_params.as_ref(),
                );
                let headers = Headers::new();
                headers.set("x-api-key", &api_key)?;
                headers.set(
                    "anthropic-version",
                    crate::providers::anthropic::API_VERSION,
                )?;
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
        let response = Fetch::Request(fetch_request).send().await?;
        Ok((response,))
    }
    .await;

    let mut llm_response = match build_fetch_result {
        Ok((resp,)) => resp,
        Err(e) => {
            let msg = format!("Failed to send request to provider: {e}");
            let _ = on_round_failed(
                kv,
                workflow_name,
                round,
                &msg,
                None,
                &provider,
                &model,
                provider_params.clone(),
                include_impl,
                &now,
            )
            .await;
            return Err(ExecutionError::Provider(msg, None));
        }
    };

    let status_code = llm_response.status_code();
    if status_code != 200 {
        let error_body = llm_response.text().await.unwrap_or_default();
        let truncated_body = if error_body.len() > 2048 {
            &error_body[..error_body.floor_char_boundary(2048)]
        } else {
            &error_body
        };
        let error_msg = format!("provider_error: HTTP {status_code}: {truncated_body}");
        let _ = on_round_failed(
            kv,
            workflow_name,
            round,
            &error_msg,
            None,
            &provider,
            &model,
            provider_params.clone(),
            include_impl,
            &now,
        )
        .await;
        return Err(ExecutionError::Provider(error_msg, None));
    }

    let raw_body = match llm_response.text().await {
        Ok(b) => b,
        Err(e) => {
            let error_msg = format!("Failed to read provider response: {e}");
            let _ = on_round_failed(
                kv,
                workflow_name,
                round,
                &error_msg,
                None,
                &provider,
                &model,
                provider_params.clone(),
                include_impl,
                &now,
            )
            .await;
            return Err(ExecutionError::Provider(error_msg, None));
        }
    };

    let events = parse_sse_events(&raw_body);
    let mut content_buffer = String::new();
    let mut usage: Option<UsageStats> = None;
    let mut stream_completed = false;

    match provider.as_str() {
        "openai" => {
            for event in &events {
                match crate::providers::openai::parse_event(event) {
                    Ok(StreamChunk::Text(t)) => content_buffer.push_str(&t),
                    Ok(StreamChunk::Usage(u)) => usage = Some(u),
                    Ok(StreamChunk::Done) => {
                        stream_completed = true;
                        break;
                    }
                    Err(e) => {
                        let partial = if content_buffer.is_empty() {
                            None
                        } else {
                            Some(content_buffer.as_str())
                        };
                        let msg = e.to_string();
                        let _ = on_round_failed(
                            kv,
                            workflow_name,
                            round,
                            &msg,
                            partial,
                            &provider,
                            &model,
                            provider_params.clone(),
                            include_impl,
                            &now,
                        )
                        .await;
                        return Err(ExecutionError::Provider(msg, None));
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
                        stream_completed = true;
                        usage = Some(crate::providers::anthropic::finalize_usage(&state));
                        break;
                    }
                    Err(e) => {
                        let partial = if content_buffer.is_empty() {
                            None
                        } else {
                            Some(content_buffer.as_str())
                        };
                        let msg = e.to_string();
                        let _ = on_round_failed(
                            kv,
                            workflow_name,
                            round,
                            &msg,
                            partial,
                            &provider,
                            &model,
                            provider_params.clone(),
                            include_impl,
                            &now,
                        )
                        .await;
                        return Err(ExecutionError::Provider(msg, None));
                    }
                }
            }
            if usage.is_none() {
                usage = Some(crate::providers::anthropic::finalize_usage(&state));
            }
        }
        _ => unreachable!(),
    }

    if content_buffer.is_empty() {
        let _ = on_round_failed(
            kv,
            workflow_name,
            round,
            "Empty response from provider",
            None,
            &provider,
            &model,
            provider_params.clone(),
            include_impl,
            &now,
        )
        .await;
        return Err(ExecutionError::Provider(
            "Empty response from provider".to_string(),
            None,
        ));
    }

    if !stream_completed {
        let bytes = content_buffer.len();
        let msg = format!("Stream ended without completion signal after {bytes} bytes");
        let _ = on_round_failed(
            kv,
            workflow_name,
            round,
            &msg,
            Some(&content_buffer),
            &provider,
            &model,
            provider_params.clone(),
            include_impl,
            &now,
        )
        .await;
        return Err(ExecutionError::Provider(
            msg,
            Some("The LLM response may have been truncated. Partial content saved. Retry with POST /run.".to_string()),
        ));
    }

    let summary = match on_round_complete(
        kv,
        workflow_name,
        round,
        &content_buffer,
        usage.clone(),
        &provider,
        &model,
        provider_params.clone(),
        include_impl,
        &now,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            let _ = release_lock(kv, workflow_name).await;
            return Err(ExecutionError::Internal(e.to_string()));
        }
    };

    Ok(RoundResult {
        content: content_buffer,
        metrics: DocumentMetrics {
            words: summary.words,
            lines: summary.lines,
            characters: summary.characters,
            headings: summary.headings,
        },
        convergence: summary.convergence,
        usage,
        provider,
        model,
        provider_params,
        include_impl,
        started_at: now,
        completed_at: summary.completed_at,
        duration_seconds: summary.duration_seconds,
        template_warnings,
    })
}

pub async fn handle(
    kv: KvStore,
    env: &Env,
    workflow: &str,
    round_str: &str,
    mut req: Request,
) -> Result<Response> {
    if let Err(resp) = validate_workflow_name(workflow) {
        return Ok(resp);
    }
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

    let default_overrides = RunOverrides {
        include_impl: None,
        skip_sequence_check: None,
        provider: None,
        model: None,
        system_prompt: None,
        provider_params: None,
    };
    let overrides: RunOverrides = match req.text().await {
        Ok(body) if !body.is_empty() => match serde_json::from_str(&body) {
            Ok(o) => o,
            Err(e) => {
                return json_error(
                    400,
                    &format!("Invalid JSON in request body: {e}"),
                    "bad_request",
                    Some("Send a valid JSON object with optional fields: include_impl, skip_sequence_check, provider, model, system_prompt, provider_params"),
                );
            }
        },
        _ => default_overrides,
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

    match execute_round(&kv, env, &wf, workflow, round, &overrides).await {
        Ok(result) => success_response(
            json!({
                "workflow": workflow,
                "round": round,
                "status": "complete",
                "content": result.content,
                "metrics": {
                    "words": result.metrics.words,
                    "lines": result.metrics.lines,
                    "characters": result.metrics.characters,
                    "headings": result.metrics.headings,
                },
                "convergence": {
                    "score": result.convergence.score,
                    "output_trend": result.convergence.output_trend,
                    "change_velocity": result.convergence.change_velocity,
                    "similarity_trend": result.convergence.similarity_trend,
                    "estimated_remaining_rounds": result.convergence.estimated_remaining_rounds,
                    "recommendation": result.convergence.recommendation,
                },
                "usage": result.usage,
                "provider": result.provider,
                "model": result.model,
                "provider_params": result.provider_params,
                "started_at": result.started_at,
                "completed_at": result.completed_at,
                "duration_seconds": result.duration_seconds,
            }),
            result.template_warnings,
            None,
        ),
        Err(e) => e.into_response(),
    }
}
