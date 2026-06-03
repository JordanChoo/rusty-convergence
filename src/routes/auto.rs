use serde::{Deserialize, Serialize};
use serde_json::json;
use worker::kv::KvStore;
use worker::wasm_bindgen::JsValue;
use worker::wasm_bindgen_futures::spawn_local;
use worker::*;

use crate::error::{json_error, now_iso8601, success_response};
use crate::routes::integrate::{
    build_integration_prompt, integrate_documents_claude, DEFAULT_INTEGRATION_MODEL,
};
use crate::routes::run::{execute_round, RoundResult};
use crate::storage::{
    autorun_key, config_key, kv_delete, kv_get, kv_list_by_prefix, kv_put, meta_key,
    parse_round_number_from_key,
};
use crate::types::{IntegrationMode, Meta, Round, RoundStatus, RunOverrides, UsageStats, Workflow};
use crate::validation::validate_workflow_name;

const DEFAULT_MAX_AUTO_ROUNDS: u32 = 20;
const MAX_ROUND_NUMBER: u32 = 999;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRotationEntry {
    pub provider: String,
    pub model: String,
    pub provider_params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct AutoRunRequest {
    pub rounds: Option<u32>,
    pub min_rounds: Option<u32>,
    pub stop_on_convergence: Option<bool>,
    pub convergence_threshold: Option<f64>,
    pub max_duration_seconds: Option<u64>,
    pub include_integration: Option<bool>,
    pub include_impl: Option<bool>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub provider_params: Option<serde_json::Value>,
    pub provider_rotation: Option<Vec<ProviderRotationEntry>>,
    pub integration_mode: Option<String>,
    pub integration_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundSummary {
    round: u32,
    words: u32,
    score: Option<f64>,
    recommendation: Option<String>,
    duration_seconds: u64,
    provider: String,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_params: Option<serde_json::Value>,
}

fn max_auto_rounds(env: &Env) -> u32 {
    env.var("MAX_AUTO_ROUNDS")
        .ok()
        .and_then(|v| v.to_string().parse().ok())
        .unwrap_or(DEFAULT_MAX_AUTO_ROUNDS)
}

fn make_overrides(req: &AutoRunRequest) -> RunOverrides {
    RunOverrides {
        include_impl: req.include_impl,
        skip_sequence_check: Some(true),
        provider: req.provider.clone(),
        model: req.model.clone(),
        system_prompt: req.system_prompt.clone(),
        provider_params: req.provider_params.clone(),
    }
}

fn make_rotated_overrides(req: &AutoRunRequest, entry: &ProviderRotationEntry) -> RunOverrides {
    RunOverrides {
        include_impl: req.include_impl,
        skip_sequence_check: Some(true),
        provider: Some(entry.provider.clone()),
        model: Some(entry.model.clone()),
        system_prompt: req.system_prompt.clone(),
        // In rotation mode, omitted entry params mean "no provider params for this
        // provider", not "fall back to the workflow-level provider params".
        provider_params: Some(entry.provider_params.clone().unwrap_or_else(|| json!({}))),
    }
}

fn rotation_index_for_round(round_num: u32, rotation_len: usize) -> usize {
    ((round_num - 1) as usize) % rotation_len
}

fn round_to_summary(round_num: u32, result: &RoundResult) -> RoundSummary {
    RoundSummary {
        round: round_num,
        words: result.metrics.words,
        score: result.convergence.score,
        recommendation: result.convergence.recommendation.clone(),
        duration_seconds: result.duration_seconds,
        provider: result.provider.clone(),
        model: result.model.clone(),
        provider_params: result.provider_params.clone(),
    }
}

fn round_to_final(round_num: u32, result: &RoundResult) -> serde_json::Value {
    json!({
        "round": round_num,
        "status": "complete",
        "words": result.metrics.words,
        "lines": result.metrics.lines,
        "characters": result.metrics.characters,
        "headings": result.metrics.headings,
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
    })
}

fn accumulate_usage(total: &mut UsageStats, round_usage: &Option<UsageStats>) {
    if let Some(u) = round_usage {
        total.input_tokens = Some(total.input_tokens.unwrap_or(0) + u.input_tokens.unwrap_or(0));
        total.output_tokens = Some(total.output_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0));
        total.reasoning_tokens =
            Some(total.reasoning_tokens.unwrap_or(0) + u.reasoning_tokens.unwrap_or(0));
    }
}

fn append_unique_warnings(warnings: &mut Vec<String>, new_warnings: &[String]) {
    for warning in new_warnings {
        if !warnings.contains(warning) {
            warnings.push(warning.clone());
        }
    }
}

fn parse_integration_mode(s: Option<&str>) -> std::result::Result<IntegrationMode, String> {
    match s {
        None | Some("none") => Ok(IntegrationMode::None),
        Some("claude") => Ok(IntegrationMode::Claude),
        Some("human") => Ok(IntegrationMode::Human),
        Some(other) => Err(format!(
            "Invalid integration_mode '{other}'. Must be 'none', 'claude', or 'human'"
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoRunState {
    pub workflow: String,
    pub integration_mode: IntegrationMode,
    pub integration_model: String,
    pub rounds_requested: u32,
    pub effective_rounds: u32,
    pub start_round: u32,
    pub min_rounds: u32,
    pub stop_on_convergence: bool,
    pub convergence_threshold: f64,
    pub max_duration_seconds: Option<u64>,
    pub include_integration: bool,
    pub include_impl: Option<bool>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub provider_params: Option<serde_json::Value>,
    pub provider_rotation: Option<Vec<ProviderRotationEntry>>,
    pub next_round: u32,
    pub last_round_num: u32,
    pub rounds_summary: Vec<RoundSummary>,
    pub total_usage: UsageStats,
    pub integration_usage: UsageStats,
    pub warnings: Vec<String>,
    pub completed_round_duration_seconds: u64,
    pub created_at: String,
    pub updated_at: String,
}

fn format_sse(event: &str, data: &serde_json::Value) -> String {
    format!("event: {event}\ndata: {}\n\n", data)
}

fn format_sse_comment(text: &str) -> String {
    format!(": {text}\n\n")
}

fn should_stop_for_duration_budget(
    elapsed_seconds: u64,
    completed_round_duration_seconds: u64,
    rounds_completed: usize,
    budget_seconds: u64,
) -> bool {
    if rounds_completed == 0 {
        return false;
    }

    let avg_round_seconds = completed_round_duration_seconds / rounds_completed as u64;
    elapsed_seconds + avg_round_seconds > budget_seconds
}

fn add_duration_budget_fields(
    data: &mut serde_json::Value,
    elapsed_seconds: u64,
    max_duration: Option<u64>,
) {
    let Some(budget_seconds) = max_duration else {
        return;
    };
    let Some(obj) = data.as_object_mut() else {
        return;
    };

    obj.insert("elapsed_seconds".to_string(), json!(elapsed_seconds));
    obj.insert("budget_seconds".to_string(), json!(budget_seconds));
    obj.insert(
        "budget_remaining_seconds".to_string(),
        json!(budget_seconds.saturating_sub(elapsed_seconds)),
    );
}

async fn write_sse(writer: &web_sys::WritableStreamDefaultWriter, text: &str) {
    let chunk = JsValue::from_str(text);
    let _ = wasm_bindgen_futures::JsFuture::from(writer.write_with_chunk(&chunk)).await;
}

async fn detect_start_round(kv: &KvStore, workflow: &str) -> Result<(u32, Vec<String>)> {
    let meta = kv_get::<Meta>(kv, &meta_key(workflow)).await?;
    let meta_latest = meta.as_ref().and_then(|m| m.latest_round).unwrap_or(0);

    let mut completed_rounds = Vec::new();
    let prefix = format!("round::{workflow}::");
    let mut cursor: Option<String> = None;
    loop {
        let (keys, next) = kv_list_by_prefix(kv, &prefix, 100, cursor.as_deref()).await?;
        for key in keys {
            let Some(round_num) = parse_round_number_from_key(&key) else {
                continue;
            };
            if !(1..=MAX_ROUND_NUMBER).contains(&round_num) {
                continue;
            }
            if matches!(
                kv_get::<Round>(kv, &key).await?,
                Some(r) if r.status == RoundStatus::Complete
            ) {
                completed_rounds.push(round_num);
            }
        }

        let Some(next_cursor) = next else {
            break;
        };
        cursor = Some(next_cursor);
    }

    completed_rounds.sort_unstable();
    completed_rounds.dedup();

    Ok(determine_start_round_from_completed(
        &completed_rounds,
        meta_latest,
    ))
}

fn determine_start_round_from_completed(
    completed_rounds: &[u32],
    meta_latest: u32,
) -> (u32, Vec<String>) {
    let mut warnings = Vec::new();
    let mut start = 1;

    for &round_num in completed_rounds {
        if round_num < start {
            continue;
        }
        if round_num == start {
            start += 1;
            continue;
        }

        warnings.push(format!(
            "Non-contiguous round history: round {round_num} is Complete after round {start} is not Complete; starting at round {start}"
        ));
        break;
    }

    let meta_start = meta_latest.saturating_add(1);
    if meta_latest > 0 && meta_start != start {
        warnings.push(format!(
            "meta.latest_round is {meta_latest}, but completed round records indicate start round {start}"
        ));
    } else if meta_latest == 0 && start > 1 {
        warnings.push(format!(
            "No meta.latest_round found, but completed round records indicate start round {start}"
        ));
    }

    (start, warnings)
}

pub async fn handle(kv: KvStore, env: &Env, workflow: &str, mut req: Request) -> Result<Response> {
    if let Err(resp) = validate_workflow_name(workflow) {
        return Ok(resp);
    }

    let wf = match kv_get::<Workflow>(&kv, &config_key(workflow)).await? {
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

    let body = match req.text().await {
        Ok(b) => b,
        Err(e) => {
            return json_error(
                400,
                &format!("Failed to read request body: {e}"),
                "bad_request",
                None,
            )
        }
    };
    if body.is_empty() {
        return json_error(
            400,
            "Request body must include 'rounds' field (integer, 1-20)",
            "bad_request",
            Some("Send a JSON body: {\"rounds\": 5}"),
        );
    }
    let auto_req: AutoRunRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_error(
                400,
                &format!("Invalid JSON in request body: {e}"),
                "bad_request",
                Some("Send a JSON body: {\"rounds\": 5}"),
            )
        }
    };

    let max_rounds = max_auto_rounds(env);
    let rounds = match auto_req.rounds {
        Some(r) if r >= 1 && r <= max_rounds => r,
        Some(r) if r < 1 => {
            return json_error(
                400,
                &format!("'rounds' must be >= 1, got {r}"),
                "bad_request",
                None,
            )
        }
        Some(r) => {
            return json_error(
                400,
                &format!("'rounds' must be <= {max_rounds}, got {r}"),
                "bad_request",
                Some(&format!("Maximum auto-run rounds is {max_rounds}")),
            )
        }
        None => {
            return json_error(
                400,
                "Request body must include 'rounds' field (integer, 1-20)",
                "bad_request",
                Some("Send a JSON body: {\"rounds\": 5}"),
            )
        }
    };

    let min_rounds = auto_req.min_rounds.unwrap_or(1);
    if min_rounds < 1 || min_rounds > rounds {
        return json_error(
            400,
            &format!("'min_rounds' must be >= 1 and <= rounds ({rounds}), got {min_rounds}"),
            "bad_request",
            None,
        );
    }

    let stop_on_convergence = auto_req.stop_on_convergence.unwrap_or(true);
    let threshold = auto_req.convergence_threshold.unwrap_or(0.90);
    if !(0.0..=1.0).contains(&threshold) {
        return json_error(
            400,
            &format!("'convergence_threshold' must be between 0.0 and 1.0, got {threshold}"),
            "bad_request",
            None,
        );
    }

    let max_duration = auto_req.max_duration_seconds;
    if let Some(d) = max_duration {
        if d < 30 {
            return json_error(
                400,
                &format!("'max_duration_seconds' must be >= 30, got {d}"),
                "bad_request",
                Some("A single LLM round typically takes 30-120 seconds"),
            );
        }
    }

    let (start_round, detection_warnings) = match detect_start_round(&kv, workflow).await {
        Ok(r) => r,
        Err(e) => return Err(e),
    };

    if start_round > MAX_ROUND_NUMBER {
        return json_error(
            422,
            "Workflow has reached the maximum round limit (999)",
            "validation_failed",
            None,
        );
    }

    let mut warnings: Vec<String> = detection_warnings;

    let effective_rounds = if start_round + rounds - 1 > MAX_ROUND_NUMBER {
        let capped = MAX_ROUND_NUMBER - start_round + 1;
        warnings.push(format!(
            "Requested {rounds} rounds but only {capped} can fit (max round: 999). Running {capped}."
        ));
        capped
    } else {
        rounds
    };

    if threshold < 0.1 {
        warnings
            .push("Low convergence_threshold may cause premature stop after round 2".to_string());
    }

    if auto_req.provider_rotation.is_some()
        && (auto_req.provider.is_some()
            || auto_req.model.is_some()
            || auto_req.provider_params.is_some())
    {
        return json_error(
            400,
            "'provider_rotation' cannot be combined with top-level 'provider', 'model', or 'provider_params'",
            "bad_request",
            Some("Use provider_rotation entries or top-level overrides, not both"),
        );
    }

    if let Some(rotation) = &auto_req.provider_rotation {
        if rotation.is_empty() {
            return json_error(
                400,
                "'provider_rotation' must have at least one entry",
                "bad_request",
                None,
            );
        }
        if rotation.len() > 10 {
            return json_error(
                400,
                "'provider_rotation' must have at most 10 entries",
                "bad_request",
                None,
            );
        }
        let mut needs_openai = false;
        let mut needs_anthropic = false;
        for (i, entry) in rotation.iter().enumerate() {
            if entry.provider != "openai" && entry.provider != "anthropic" {
                return json_error(
                    400,
                    &format!(
                        "provider_rotation[{i}]: invalid provider '{}'. Must be 'openai' or 'anthropic'",
                        entry.provider
                    ),
                    "bad_request",
                    None,
                );
            }
            if entry.model.trim().is_empty() {
                return json_error(
                    400,
                    &format!("provider_rotation[{i}]: model must not be empty"),
                    "bad_request",
                    None,
                );
            }
            if entry.provider == "openai" {
                needs_openai = true;
            }
            if entry.provider == "anthropic" {
                needs_anthropic = true;
            }
        }
        if needs_openai && env.secret("OPENAI_API_KEY").is_err() {
            return json_error(
                500,
                "Missing secret required by provider_rotation: OPENAI_API_KEY",
                "missing_config",
                Some("Configure the OPENAI_API_KEY Worker secret before using this rotation"),
            );
        }
        if needs_anthropic && env.secret("ANTHROPIC_API_KEY").is_err() {
            return json_error(
                500,
                "Missing secret required by provider_rotation: ANTHROPIC_API_KEY",
                "missing_config",
                Some("Configure the ANTHROPIC_API_KEY Worker secret before using this rotation"),
            );
        }
    }

    let integration_mode = match parse_integration_mode(auto_req.integration_mode.as_deref()) {
        Ok(m) => m,
        Err(msg) => return json_error(400, &msg, "bad_request", None),
    };

    let integration_model = auto_req
        .integration_model
        .clone()
        .unwrap_or_else(|| DEFAULT_INTEGRATION_MODEL.to_string());

    if integration_mode == IntegrationMode::Claude {
        if env.secret("ANTHROPIC_API_KEY").is_err() {
            return json_error(
                500,
                "Missing ANTHROPIC_API_KEY required for Claude integration mode",
                "missing_config",
                Some("Configure the ANTHROPIC_API_KEY Worker secret"),
            );
        }
    }

    if integration_mode == IntegrationMode::Human {
        if let Ok(Some(_)) = kv_get::<AutoRunState>(&kv, &autorun_key(workflow)).await {
            return json_error(
                409,
                &format!("An auto-run for workflow '{workflow}' is already awaiting integration"),
                "conflict",
                Some(&format!(
                    "Call POST /auto/{workflow}/resume to continue, or DELETE /workflows/{workflow} to reset"
                )),
            );
        }
    }

    let overrides = make_overrides(&auto_req);
    let rotation = auto_req.provider_rotation.clone();

    let accept = req
        .headers()
        .get("Accept")
        .ok()
        .flatten()
        .unwrap_or_default();
    let wants_json = accept.contains("application/json");
    let include_integration = auto_req.include_integration.unwrap_or(false);

    if integration_mode == IntegrationMode::Human && !wants_json {
        return json_error(
            400,
            "Human integration mode requires JSON responses",
            "bad_request",
            Some("Add 'Accept: application/json' header, or use integration_mode 'claude' for streaming"),
        );
    }

    if wants_json {
        handle_json(
            &kv,
            env,
            &wf,
            workflow,
            start_round,
            effective_rounds,
            rounds,
            min_rounds,
            stop_on_convergence,
            threshold,
            max_duration,
            include_integration,
            &overrides,
            &rotation,
            &auto_req,
            warnings,
            &integration_mode,
            &integration_model,
        )
        .await
    } else {
        handle_sse(
            kv,
            env.clone(),
            wf,
            workflow.to_string(),
            start_round,
            effective_rounds,
            rounds,
            min_rounds,
            stop_on_convergence,
            threshold,
            max_duration,
            include_integration,
            overrides,
            rotation,
            auto_req,
            warnings,
            integration_mode,
            integration_model,
        )
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_json(
    kv: &KvStore,
    env: &Env,
    wf: &Workflow,
    workflow: &str,
    start_round: u32,
    effective_rounds: u32,
    rounds_requested: u32,
    min_rounds: u32,
    stop_on_convergence: bool,
    threshold: f64,
    max_duration: Option<u64>,
    include_integration: bool,
    overrides: &RunOverrides,
    rotation: &Option<Vec<ProviderRotationEntry>>,
    auto_req: &AutoRunRequest,
    mut warnings: Vec<String>,
    integration_mode: &IntegrationMode,
    integration_model: &str,
) -> Result<Response> {
    let batch_start = now_iso8601();
    let batch_start_ms = Date::now().as_millis();
    let mut summaries: Vec<RoundSummary> = Vec::new();
    let mut completed_round_duration_seconds = 0;
    let mut total_usage = UsageStats {
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
    };
    let mut integration_usage = UsageStats {
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
    };
    let mut final_round_data: Option<serde_json::Value> = None;
    let mut final_round_content: Option<String> = None;
    let mut stopped_reason = "completed".to_string();
    let mut error_detail: Option<String> = None;
    let mut last_round_num = start_round;

    let mut round_num = start_round;
    while summaries.len() < effective_rounds as usize && round_num <= MAX_ROUND_NUMBER {
        if let Some(budget) = max_duration {
            if !summaries.is_empty() {
                let elapsed_ms = Date::now().as_millis() - batch_start_ms;
                let elapsed_s = elapsed_ms / 1000;
                if should_stop_for_duration_budget(
                    elapsed_s,
                    completed_round_duration_seconds,
                    summaries.len(),
                    budget,
                ) {
                    stopped_reason = "duration_limit".to_string();
                    break;
                }
            }
        }

        let effective_overrides = if let Some(rot) = rotation {
            let idx = rotation_index_for_round(round_num, rot.len());
            make_rotated_overrides(auto_req, &rot[idx])
        } else {
            overrides.clone()
        };

        match execute_round(kv, env, wf, workflow, round_num, &effective_overrides).await {
            Ok(result) => {
                let summary = round_to_summary(round_num, &result);
                final_round_data = Some(round_to_final(round_num, &result));
                final_round_content = Some(result.content.clone());
                accumulate_usage(&mut total_usage, &result.usage);
                completed_round_duration_seconds += result.duration_seconds;
                last_round_num = round_num;

                let converged = stop_on_convergence
                    && summaries.len() + 1 >= min_rounds as usize
                    && result.convergence.score.is_some_and(|s| s >= threshold);

                append_unique_warnings(&mut warnings, &result.template_warnings);

                summaries.push(summary);

                if converged {
                    stopped_reason = "convergence".to_string();
                    break;
                }

                let is_last_round = summaries.len() >= effective_rounds as usize
                    || round_num + 1 > MAX_ROUND_NUMBER;

                if !is_last_round {
                    match integration_mode {
                        IntegrationMode::Claude => {
                            match integrate_documents_claude(
                                kv,
                                env,
                                workflow,
                                &result.content,
                                &wf.documents,
                                integration_model,
                            )
                            .await
                            {
                                Ok(int_result) => {
                                    accumulate_usage(
                                        &mut integration_usage,
                                        &Some(int_result.usage),
                                    );
                                }
                                Err(e) => {
                                    let detail =
                                        format!("Integration after round {round_num} failed: {e}");
                                    stopped_reason = "integration_error".to_string();
                                    error_detail = Some(detail);
                                    break;
                                }
                            }
                        }
                        IntegrationMode::Human => {
                            let state = AutoRunState {
                                workflow: workflow.to_string(),
                                integration_mode: IntegrationMode::Human,
                                integration_model: integration_model.to_string(),
                                rounds_requested,
                                effective_rounds,
                                start_round,
                                min_rounds,
                                stop_on_convergence,
                                convergence_threshold: threshold,
                                max_duration_seconds: max_duration,
                                include_integration,
                                include_impl: auto_req.include_impl,
                                provider: auto_req.provider.clone(),
                                model: auto_req.model.clone(),
                                system_prompt: auto_req.system_prompt.clone(),
                                provider_params: auto_req.provider_params.clone(),
                                provider_rotation: rotation.clone(),
                                next_round: round_num + 1,
                                last_round_num: round_num,
                                rounds_summary: summaries.clone(),
                                total_usage: total_usage.clone(),
                                integration_usage: integration_usage.clone(),
                                warnings: warnings.clone(),
                                completed_round_duration_seconds,
                                created_at: batch_start.clone(),
                                updated_at: now_iso8601(),
                            };
                            kv_put(kv, &autorun_key(workflow), &state).await?;

                            let elapsed_seconds = (Date::now().as_millis() - batch_start_ms) / 1000;
                            let mut data = json!({
                                "workflow": workflow,
                                "rounds_completed": summaries.len(),
                                "rounds_requested": rounds_requested,
                                "start_round": start_round,
                                "final_round_number": round_num,
                                "stopped_reason": "awaiting_integration",
                                "final_round": final_round_data,
                                "rounds_summary": summaries,
                                "total_usage": total_usage,
                                "total_duration_seconds": elapsed_seconds,
                                "next_round": round_num + 1,
                                "integration_mode": "human",
                            });
                            add_duration_budget_fields(&mut data, elapsed_seconds, max_duration);

                            return success_response(
                                data,
                                warnings,
                                Some(&format!(
                                    "Update documents via PUT /documents/{workflow}/{{role}}, then POST /auto/{workflow}/resume to continue"
                                )),
                            );
                        }
                        IntegrationMode::None => {}
                    }
                }
            }
            Err(e) if e.is_round_already_complete() => {
                last_round_num = round_num;
                round_num += 1;
                continue;
            }
            Err(e) => {
                if summaries.is_empty() {
                    return e.into_response();
                }
                let detail = format!("Round {round_num} failed: {e:?}");
                stopped_reason = "error".to_string();
                warnings.push(format!(
                    "{detail}. {} rounds completed before failure.",
                    summaries.len()
                ));
                error_detail = Some(detail);
                break;
            }
        }
        round_num += 1;
    }

    let _ = kv_delete(kv, &autorun_key(workflow)).await;

    let batch_end = now_iso8601();
    let total_duration = compute_batch_duration(&batch_start, &batch_end);
    let elapsed_seconds = (Date::now().as_millis() - batch_start_ms) / 1000;

    let mut data = json!({
        "workflow": workflow,
        "rounds_completed": summaries.len(),
        "rounds_requested": rounds_requested,
        "start_round": start_round,
        "final_round_number": last_round_num,
        "stopped_reason": stopped_reason,
        "final_round": final_round_data,
        "rounds_summary": summaries,
        "total_usage": total_usage,
        "total_duration_seconds": total_duration,
    });
    if let Some(ref detail) = error_detail {
        data["error_detail"] = json!(detail);
    }
    if *integration_mode != IntegrationMode::None {
        data["integration_mode"] = json!(integration_mode);
        data["integration_usage"] = json!(integration_usage);
    }
    add_duration_budget_fields(&mut data, elapsed_seconds, max_duration);

    if include_integration {
        if let Some(content) = &final_round_content {
            data["integration_prompt"] =
                json!(build_integration_prompt(workflow, last_round_num, content));
        }
    }

    let hint = if error_detail.is_some() {
        Some(format!(
            "Use POST /auto/{workflow} to resume from round {round_num}, or POST /run/{workflow}/{round_num} to retry the failed round individually."
        ))
    } else {
        None
    };

    success_response(data, warnings, hint.as_deref())
}

#[allow(clippy::too_many_arguments)]
fn handle_sse(
    kv: KvStore,
    env: Env,
    wf: Workflow,
    workflow: String,
    start_round: u32,
    effective_rounds: u32,
    rounds_requested: u32,
    min_rounds: u32,
    stop_on_convergence: bool,
    threshold: f64,
    max_duration: Option<u64>,
    include_integration: bool,
    overrides: RunOverrides,
    rotation: Option<Vec<ProviderRotationEntry>>,
    auto_req: AutoRunRequest,
    warnings: Vec<String>,
    integration_mode: IntegrationMode,
    integration_model: String,
) -> Result<Response> {
    let transform = web_sys::TransformStream::new()
        .map_err(|e| Error::JsError(format!("TransformStream::new failed: {e:?}")))?;
    let readable = transform.readable();
    let writable = transform.writable();
    let writer = writable
        .get_writer()
        .map_err(|e| Error::JsError(format!("get_writer failed: {e:?}")))?;

    spawn_local(async move {
        write_sse(&writer, &format_sse_comment("connected")).await;

        let mut warnings = warnings;
        if !warnings.is_empty() {
            write_sse(
                &writer,
                &format_sse("warnings", &json!({ "warnings": warnings.clone() })),
            )
            .await;
        }

        let batch_start_ms = Date::now().as_millis();
        let mut summaries: Vec<RoundSummary> = Vec::new();
        let mut completed_round_duration_seconds = 0;
        let mut total_usage = UsageStats {
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
        };
        let mut final_round_data: Option<serde_json::Value> = None;
        let mut final_round_content: Option<String> = None;
        let mut stopped_reason = "completed".to_string();
        let mut error_detail: Option<String> = None;
        let mut last_round_num = start_round;

        let mut round_num = start_round;
        while summaries.len() < effective_rounds as usize && round_num <= MAX_ROUND_NUMBER {
            if let Some(budget) = max_duration {
                if !summaries.is_empty() {
                    let elapsed_ms = Date::now().as_millis() - batch_start_ms;
                    let elapsed_s = elapsed_ms / 1000;
                    if should_stop_for_duration_budget(
                        elapsed_s,
                        completed_round_duration_seconds,
                        summaries.len(),
                        budget,
                    ) {
                        stopped_reason = "duration_limit".to_string();
                        break;
                    }
                }
            }

            write_sse(
                &writer,
                &format_sse(
                    "round_start",
                    &json!({
                        "round": round_num,
                        "started_at": now_iso8601(),
                    }),
                ),
            )
            .await;

            let effective_overrides = if let Some(rot) = &rotation {
                let idx = rotation_index_for_round(round_num, rot.len());
                make_rotated_overrides(&auto_req, &rot[idx])
            } else {
                overrides.clone()
            };

            match execute_round(&kv, &env, &wf, &workflow, round_num, &effective_overrides).await {
                Ok(result) => {
                    let summary = round_to_summary(round_num, &result);

                    write_sse(
                        &writer,
                        &format_sse(
                            "round_complete",
                            &json!({
                                "round": round_num,
                                "words": result.metrics.words,
                                "score": result.convergence.score,
                                "recommendation": result.convergence.recommendation,
                                "duration_seconds": result.duration_seconds,
                                "provider": result.provider,
                                "model": result.model,
                                "provider_params": result.provider_params,
                            }),
                        ),
                    )
                    .await;

                    final_round_data = Some(round_to_final(round_num, &result));
                    final_round_content = Some(result.content.clone());
                    accumulate_usage(&mut total_usage, &result.usage);
                    append_unique_warnings(&mut warnings, &result.template_warnings);
                    completed_round_duration_seconds += result.duration_seconds;
                    last_round_num = round_num;

                    let converged = stop_on_convergence
                        && summaries.len() + 1 >= min_rounds as usize
                        && result.convergence.score.is_some_and(|s| s >= threshold);

                    summaries.push(summary);

                    if converged {
                        stopped_reason = "convergence".to_string();
                        break;
                    }

                    let is_last_round = summaries.len() >= effective_rounds as usize
                        || round_num + 1 > MAX_ROUND_NUMBER;

                    if !is_last_round && integration_mode == IntegrationMode::Claude {
                        write_sse(
                            &writer,
                            &format_sse("integration_start", &json!({"round": round_num})),
                        )
                        .await;

                        match integrate_documents_claude(
                            &kv,
                            &env,
                            &workflow,
                            &result.content,
                            &wf.documents,
                            &integration_model,
                        )
                        .await
                        {
                            Ok(int_result) => {
                                write_sse(
                                    &writer,
                                    &format_sse(
                                        "integration_complete",
                                        &json!({
                                            "round": round_num,
                                            "documents_updated": int_result.documents_updated,
                                            "duration_seconds": int_result.duration_seconds,
                                        }),
                                    ),
                                )
                                .await;
                            }
                            Err(e) => {
                                let detail =
                                    format!("Integration after round {round_num} failed: {e}");
                                write_sse(
                                    &writer,
                                    &format_sse(
                                        "integration_error",
                                        &json!({
                                            "round": round_num,
                                            "error": format!("{e}"),
                                        }),
                                    ),
                                )
                                .await;
                                stopped_reason = "integration_error".to_string();
                                error_detail = Some(detail);
                                break;
                            }
                        }
                    }
                }
                Err(e) if e.is_round_already_complete() => {
                    last_round_num = round_num;
                    round_num += 1;
                    continue;
                }
                Err(e) => {
                    let detail = format!("Round {round_num} failed: {e:?}");
                    write_sse(
                        &writer,
                        &format_sse(
                            "error",
                            &json!({
                                "round": round_num,
                                "error": format!("{e:?}"),
                                "rounds_completed": summaries.len(),
                            }),
                        ),
                    )
                    .await;
                    stopped_reason = "error".to_string();
                    error_detail = Some(detail);
                    break;
                }
            }
            round_num += 1;
        }

        let total_elapsed_s = (Date::now().as_millis() - batch_start_ms) / 1000;
        let mut done_data = json!({
            "workflow": workflow,
            "rounds_completed": summaries.len(),
            "rounds_requested": rounds_requested,
            "start_round": start_round,
            "final_round_number": last_round_num,
            "stopped_reason": stopped_reason,
            "final_round": final_round_data,
            "rounds_summary": summaries,
            "total_usage": total_usage,
            "total_duration_seconds": total_elapsed_s,
            "warnings": warnings,
        });
        if let Some(ref detail) = error_detail {
            done_data["error_detail"] = json!(detail);
        }
        add_duration_budget_fields(&mut done_data, total_elapsed_s, max_duration);
        if include_integration {
            if let Some(content) = &final_round_content {
                done_data["integration_prompt"] =
                    json!(build_integration_prompt(&workflow, last_round_num, content));
            }
        }
        write_sse(&writer, &format_sse("done", &done_data)).await;

        let _ = wasm_bindgen_futures::JsFuture::from(writer.close()).await;
    });

    let resp_init = web_sys::ResponseInit::new();
    resp_init.set_status(200);
    let headers = web_sys::Headers::new()
        .map_err(|e| Error::JsError(format!("Headers::new failed: {e:?}")))?;
    headers
        .set("Content-Type", "text/event-stream")
        .map_err(|e| Error::JsError(format!("set Content-Type failed: {e:?}")))?;
    headers
        .set("Cache-Control", "no-cache")
        .map_err(|e| Error::JsError(format!("set Cache-Control failed: {e:?}")))?;
    resp_init.set_headers(&headers.into());

    let js_resp =
        web_sys::Response::new_with_opt_readable_stream_and_init(Some(&readable), &resp_init)
            .map_err(|e| Error::JsError(format!("Response::new failed: {e:?}")))?;

    Ok(js_resp.into())
}

pub async fn handle_resume(
    kv: KvStore,
    env: &Env,
    workflow: &str,
    _req: Request,
) -> Result<Response> {
    if let Err(resp) = validate_workflow_name(workflow) {
        return Ok(resp);
    }

    let state: AutoRunState = match kv_get(&kv, &autorun_key(workflow)).await? {
        Some(s) => s,
        None => {
            return json_error(
                404,
                &format!("No pending auto-run found for workflow '{workflow}'"),
                "not_found",
                Some(&format!(
                    "Start an auto-run with POST /auto/{workflow} using integration_mode 'human'"
                )),
            );
        }
    };

    let wf = match kv_get::<Workflow>(&kv, &config_key(workflow)).await? {
        Some(w) => w,
        None => {
            let _ = kv_delete(&kv, &autorun_key(workflow)).await;
            return json_error(
                404,
                &format!("Workflow '{workflow}' no longer exists"),
                "not_found",
                None,
            );
        }
    };

    let mut summaries = state.rounds_summary;
    let mut completed_round_duration_seconds = state.completed_round_duration_seconds;
    let mut total_usage = state.total_usage;
    let integration_usage = state.integration_usage;
    let mut warnings = state.warnings;
    let mut final_round_data: Option<serde_json::Value> = None;
    let mut final_round_content: Option<String> = None;
    let mut stopped_reason = "completed".to_string();
    let mut error_detail: Option<String> = None;
    let mut last_round_num = state.last_round_num;

    let base_overrides = RunOverrides {
        include_impl: state.include_impl,
        skip_sequence_check: Some(true),
        provider: state.provider.clone(),
        model: state.model.clone(),
        system_prompt: state.system_prompt.clone(),
        provider_params: state.provider_params.clone(),
    };

    let mut round_num = state.next_round;
    while summaries.len() < state.effective_rounds as usize && round_num <= MAX_ROUND_NUMBER {
        if let Some(budget) = state.max_duration_seconds {
            if !summaries.is_empty() {
                let elapsed_s = completed_round_duration_seconds;
                if should_stop_for_duration_budget(
                    elapsed_s,
                    completed_round_duration_seconds,
                    summaries.len(),
                    budget,
                ) {
                    stopped_reason = "duration_limit".to_string();
                    break;
                }
            }
        }

        let effective_overrides = if let Some(ref rot) = state.provider_rotation {
            let idx = rotation_index_for_round(round_num, rot.len());
            RunOverrides {
                include_impl: state.include_impl,
                skip_sequence_check: Some(true),
                provider: Some(rot[idx].provider.clone()),
                model: Some(rot[idx].model.clone()),
                system_prompt: state.system_prompt.clone(),
                provider_params: Some(
                    rot[idx]
                        .provider_params
                        .clone()
                        .unwrap_or_else(|| json!({})),
                ),
            }
        } else {
            base_overrides.clone()
        };

        match execute_round(&kv, env, &wf, workflow, round_num, &effective_overrides).await {
            Ok(result) => {
                let summary = round_to_summary(round_num, &result);
                final_round_data = Some(round_to_final(round_num, &result));
                final_round_content = Some(result.content.clone());
                accumulate_usage(&mut total_usage, &result.usage);
                completed_round_duration_seconds += result.duration_seconds;
                last_round_num = round_num;

                let converged = state.stop_on_convergence
                    && summaries.len() + 1 >= state.min_rounds as usize
                    && result
                        .convergence
                        .score
                        .is_some_and(|s| s >= state.convergence_threshold);

                append_unique_warnings(&mut warnings, &result.template_warnings);
                summaries.push(summary);

                if converged {
                    stopped_reason = "convergence".to_string();
                    break;
                }

                let is_last_round = summaries.len() >= state.effective_rounds as usize
                    || round_num + 1 > MAX_ROUND_NUMBER;

                if !is_last_round {
                    // Human mode: pause again for next integration
                    let updated_state = AutoRunState {
                        workflow: workflow.to_string(),
                        integration_mode: IntegrationMode::Human,
                        integration_model: state.integration_model.clone(),
                        rounds_requested: state.rounds_requested,
                        effective_rounds: state.effective_rounds,
                        start_round: state.start_round,
                        min_rounds: state.min_rounds,
                        stop_on_convergence: state.stop_on_convergence,
                        convergence_threshold: state.convergence_threshold,
                        max_duration_seconds: state.max_duration_seconds,
                        include_integration: state.include_integration,
                        include_impl: state.include_impl,
                        provider: state.provider.clone(),
                        model: state.model.clone(),
                        system_prompt: state.system_prompt.clone(),
                        provider_params: state.provider_params.clone(),
                        provider_rotation: state.provider_rotation.clone(),
                        next_round: round_num + 1,
                        last_round_num: round_num,
                        rounds_summary: summaries.clone(),
                        total_usage: total_usage.clone(),
                        integration_usage: integration_usage.clone(),
                        warnings: warnings.clone(),
                        completed_round_duration_seconds,
                        created_at: state.created_at.clone(),
                        updated_at: now_iso8601(),
                    };
                    kv_put(&kv, &autorun_key(workflow), &updated_state).await?;

                    let mut data = json!({
                        "workflow": workflow,
                        "rounds_completed": summaries.len(),
                        "rounds_requested": state.rounds_requested,
                        "start_round": state.start_round,
                        "final_round_number": round_num,
                        "stopped_reason": "awaiting_integration",
                        "final_round": final_round_data,
                        "rounds_summary": summaries,
                        "total_usage": total_usage,
                        "total_duration_seconds": completed_round_duration_seconds,
                        "next_round": round_num + 1,
                        "integration_mode": "human",
                    });
                    add_duration_budget_fields(
                        &mut data,
                        completed_round_duration_seconds,
                        state.max_duration_seconds,
                    );

                    return success_response(
                        data,
                        warnings,
                        Some(&format!(
                            "Update documents via PUT /documents/{workflow}/{{role}}, then POST /auto/{workflow}/resume to continue"
                        )),
                    );
                }
            }
            Err(e) if e.is_round_already_complete() => {
                last_round_num = round_num;
                round_num += 1;
                continue;
            }
            Err(e) => {
                if summaries.is_empty() {
                    let _ = kv_delete(&kv, &autorun_key(workflow)).await;
                    return e.into_response();
                }
                let detail = format!("Round {round_num} failed: {e:?}");
                stopped_reason = "error".to_string();
                warnings.push(format!(
                    "{detail}. {} rounds completed before failure.",
                    summaries.len()
                ));
                error_detail = Some(detail);
                break;
            }
        }
        round_num += 1;
    }

    let _ = kv_delete(&kv, &autorun_key(workflow)).await;

    let mut data = json!({
        "workflow": workflow,
        "rounds_completed": summaries.len(),
        "rounds_requested": state.rounds_requested,
        "start_round": state.start_round,
        "final_round_number": last_round_num,
        "stopped_reason": stopped_reason,
        "final_round": final_round_data,
        "rounds_summary": summaries,
        "total_usage": total_usage,
        "total_duration_seconds": completed_round_duration_seconds,
        "integration_mode": "human",
        "integration_usage": integration_usage,
    });
    if let Some(ref detail) = error_detail {
        data["error_detail"] = json!(detail);
    }
    add_duration_budget_fields(
        &mut data,
        completed_round_duration_seconds,
        state.max_duration_seconds,
    );

    if state.include_integration {
        if let Some(content) = &final_round_content {
            data["integration_prompt"] =
                json!(build_integration_prompt(workflow, last_round_num, content));
        }
    }

    let hint = if error_detail.is_some() {
        Some(format!(
            "Use POST /auto/{workflow} to start a new auto-run, or POST /run/{workflow}/{round_num} to retry individually."
        ))
    } else {
        None
    };

    success_response(data, warnings, hint.as_deref())
}

fn compute_batch_duration(started_at: &str, completed_at: &str) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_budget_allows_first_round() {
        assert!(!should_stop_for_duration_budget(45, 0, 0, 30));
    }

    #[test]
    fn provider_rotation_uses_absolute_round_numbering() {
        assert_eq!(rotation_index_for_round(1, 2), 0);
        assert_eq!(rotation_index_for_round(2, 2), 1);
        assert_eq!(rotation_index_for_round(3, 2), 0);
        assert_eq!(rotation_index_for_round(4, 2), 1);
    }

    #[test]
    fn provider_rotation_resume_preserves_long_run_order() {
        assert_eq!(rotation_index_for_round(4, 3), 0);
        assert_eq!(rotation_index_for_round(5, 3), 1);
        assert_eq!(rotation_index_for_round(6, 3), 2);
    }

    #[test]
    fn rotated_overrides_do_not_inherit_workflow_provider_params() {
        let req = AutoRunRequest {
            rounds: Some(1),
            min_rounds: None,
            stop_on_convergence: None,
            convergence_threshold: None,
            max_duration_seconds: None,
            include_integration: None,
            include_impl: None,
            provider: None,
            model: None,
            system_prompt: None,
            provider_params: Some(json!({"reasoning_effort": "high"})),
            provider_rotation: None,
            integration_mode: None,
            integration_model: None,
        };
        let entry = ProviderRotationEntry {
            provider: "anthropic".to_string(),
            model: "claude-opus-4-6".to_string(),
            provider_params: None,
        };

        let overrides = make_rotated_overrides(&req, &entry);

        assert_eq!(overrides.provider.as_deref(), Some("anthropic"));
        assert_eq!(overrides.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(overrides.provider_params, Some(json!({})));
    }

    #[test]
    fn append_unique_warnings_deduplicates_existing_entries() {
        let mut warnings = vec!["initial warning".to_string(), "shared warning".to_string()];
        let new_warnings = vec!["shared warning".to_string(), "template warning".to_string()];

        append_unique_warnings(&mut warnings, &new_warnings);

        assert_eq!(
            warnings,
            vec![
                "initial warning".to_string(),
                "shared warning".to_string(),
                "template warning".to_string()
            ]
        );
    }

    #[test]
    fn start_detection_uses_first_gap_even_when_meta_points_later() {
        let (start, warnings) = determine_start_round_from_completed(&[1, 2, 3, 5], 5);

        assert_eq!(start, 4);
        assert!(warnings
            .iter()
            .any(|w| w.contains("Non-contiguous round history")));
        assert!(warnings.iter().any(|w| w.contains("meta.latest_round")));
    }

    #[test]
    fn start_detection_resumes_after_contiguous_completed_rounds_without_meta() {
        let (start, warnings) = determine_start_round_from_completed(&[1, 2, 3], 0);

        assert_eq!(start, 4);
        assert!(warnings.iter().any(|w| w.contains("No meta.latest_round")));
    }

    #[test]
    fn start_detection_returns_max_plus_one_when_all_rounds_complete() {
        let completed_rounds: Vec<u32> = (1..=MAX_ROUND_NUMBER).collect();

        let (start, warnings) = determine_start_round_from_completed(&completed_rounds, 999);

        assert_eq!(start, MAX_ROUND_NUMBER + 1);
        assert!(warnings.is_empty());
    }

    #[test]
    fn duration_budget_stops_when_next_round_would_exceed_budget() {
        assert!(should_stop_for_duration_budget(90, 90, 2, 120));
    }

    #[test]
    fn duration_budget_continues_when_next_round_fits_budget() {
        assert!(!should_stop_for_duration_budget(60, 60, 2, 120));
    }

    #[test]
    fn duration_budget_fields_are_absent_without_budget() {
        let mut data = json!({});

        add_duration_budget_fields(&mut data, 75, None);

        assert!(data.get("elapsed_seconds").is_none());
        assert!(data.get("budget_seconds").is_none());
        assert!(data.get("budget_remaining_seconds").is_none());
    }

    #[test]
    fn duration_budget_fields_include_remaining_time() {
        let mut data = json!({});

        add_duration_budget_fields(&mut data, 75, Some(120));

        assert_eq!(data["elapsed_seconds"], json!(75));
        assert_eq!(data["budget_seconds"], json!(120));
        assert_eq!(data["budget_remaining_seconds"], json!(45));
    }

    #[test]
    fn duration_budget_remaining_time_saturates_at_zero() {
        let mut data = json!({});

        add_duration_budget_fields(&mut data, 125, Some(120));

        assert_eq!(data["budget_remaining_seconds"], json!(0));
    }

    #[test]
    fn parse_integration_mode_defaults_to_none() {
        assert_eq!(parse_integration_mode(None).unwrap(), IntegrationMode::None);
        assert_eq!(
            parse_integration_mode(Some("none")).unwrap(),
            IntegrationMode::None
        );
    }

    #[test]
    fn parse_integration_mode_accepts_valid_values() {
        assert_eq!(
            parse_integration_mode(Some("claude")).unwrap(),
            IntegrationMode::Claude
        );
        assert_eq!(
            parse_integration_mode(Some("human")).unwrap(),
            IntegrationMode::Human
        );
    }

    #[test]
    fn parse_integration_mode_rejects_invalid_values() {
        assert!(parse_integration_mode(Some("auto")).is_err());
        assert!(parse_integration_mode(Some("")).is_err());
        assert!(parse_integration_mode(Some("CLAUDE")).is_err());
    }

    #[test]
    fn autorun_state_serialization_roundtrip() {
        let state = AutoRunState {
            workflow: "demo".to_string(),
            integration_mode: IntegrationMode::Human,
            integration_model: "claude-sonnet-4-6".to_string(),
            rounds_requested: 10,
            effective_rounds: 10,
            start_round: 1,
            min_rounds: 2,
            stop_on_convergence: true,
            convergence_threshold: 0.90,
            max_duration_seconds: Some(300),
            include_integration: false,
            include_impl: None,
            provider: None,
            model: None,
            system_prompt: None,
            provider_params: None,
            provider_rotation: None,
            next_round: 4,
            last_round_num: 3,
            rounds_summary: vec![],
            total_usage: UsageStats {
                input_tokens: Some(1000),
                output_tokens: Some(500),
                reasoning_tokens: None,
            },
            integration_usage: UsageStats {
                input_tokens: None,
                output_tokens: None,
                reasoning_tokens: None,
            },
            warnings: vec!["test warning".to_string()],
            completed_round_duration_seconds: 90,
            created_at: "2026-06-04T00:00:00Z".to_string(),
            updated_at: "2026-06-04T00:01:00Z".to_string(),
        };

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: AutoRunState = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.workflow, "demo");
        assert_eq!(deserialized.integration_mode, IntegrationMode::Human);
        assert_eq!(deserialized.next_round, 4);
        assert_eq!(deserialized.total_usage.input_tokens, Some(1000));
        assert_eq!(deserialized.warnings.len(), 1);
    }
}
