use serde::{Deserialize, Serialize};
use serde_json::json;
use worker::kv::KvStore;
use worker::wasm_bindgen::JsValue;
use worker::wasm_bindgen_futures::spawn_local;
use worker::*;

use crate::error::{json_error, now_iso8601, success_response};
use crate::routes::run::{execute_round, RoundResult};
use crate::storage::{config_key, kv_get, meta_key};
use crate::types::{Meta, RunOverrides, UsageStats, Workflow};
use crate::validation::validate_workflow_name;

const DEFAULT_MAX_AUTO_ROUNDS: u32 = 20;
const MAX_ROUND_NUMBER: u32 = 999;

#[derive(Debug, Deserialize)]
pub struct AutoRunRequest {
    pub rounds: Option<u32>,
    pub min_rounds: Option<u32>,
    pub stop_on_convergence: Option<bool>,
    pub convergence_threshold: Option<f64>,
    pub include_impl: Option<bool>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub provider_params: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct RoundSummary {
    round: u32,
    words: u32,
    score: Option<f64>,
    recommendation: Option<String>,
    duration_seconds: u64,
    provider: String,
    model: String,
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

fn round_to_summary(round_num: u32, result: &RoundResult) -> RoundSummary {
    RoundSummary {
        round: round_num,
        words: result.metrics.words,
        score: result.convergence.score,
        recommendation: result.convergence.recommendation.clone(),
        duration_seconds: result.duration_seconds,
        provider: result.provider.clone(),
        model: result.model.clone(),
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

fn format_sse(event: &str, data: &serde_json::Value) -> String {
    format!("event: {event}\ndata: {}\n\n", data)
}

fn format_sse_comment(text: &str) -> String {
    format!(": {text}\n\n")
}

async fn write_sse(writer: &web_sys::WritableStreamDefaultWriter, text: &str) {
    let chunk = JsValue::from_str(text);
    let _ = wasm_bindgen_futures::JsFuture::from(writer.write_with_chunk(&chunk)).await;
}

async fn detect_start_round(kv: &KvStore, workflow: &str) -> Result<u32> {
    match kv_get::<Meta>(kv, &meta_key(workflow)).await? {
        Some(meta) => Ok(meta.latest_round.map_or(1, |r| r + 1)),
        None => Ok(1),
    }
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

    let start_round = match detect_start_round(&kv, workflow).await {
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

    let mut warnings: Vec<String> = Vec::new();

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

    let overrides = make_overrides(&auto_req);

    let accept = req
        .headers()
        .get("Accept")
        .ok()
        .flatten()
        .unwrap_or_default();
    let wants_json = accept.contains("application/json");

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
            &overrides,
            warnings,
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
            overrides,
            warnings,
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
    overrides: &RunOverrides,
    mut warnings: Vec<String>,
) -> Result<Response> {
    let batch_start = now_iso8601();
    let mut summaries: Vec<RoundSummary> = Vec::new();
    let mut total_usage = UsageStats {
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
    };
    let mut final_round_data: Option<serde_json::Value> = None;
    let mut stopped_reason = "completed".to_string();
    let mut last_round_num = start_round;

    for i in 0..effective_rounds {
        let round_num = start_round + i;
        last_round_num = round_num;

        match execute_round(kv, env, wf, workflow, round_num, overrides).await {
            Ok(result) => {
                let summary = round_to_summary(round_num, &result);
                final_round_data = Some(round_to_final(round_num, &result));
                accumulate_usage(&mut total_usage, &result.usage);

                let converged = stop_on_convergence
                    && summaries.len() + 1 >= min_rounds as usize
                    && result.convergence.score.map_or(false, |s| s >= threshold);

                for w in &result.template_warnings {
                    if !warnings.contains(w) {
                        warnings.push(w.clone());
                    }
                }

                summaries.push(summary);

                if converged {
                    stopped_reason = "convergence".to_string();
                    break;
                }
            }
            Err(e) => {
                if summaries.is_empty() {
                    return e.into_response();
                }
                stopped_reason = "error".to_string();
                warnings.push(format!("Round {round_num} failed: {e:?}"));
                warnings.push(format!(
                    "Resume with POST /run/{workflow}/{round_num} or POST /auto/{workflow}"
                ));
                break;
            }
        }
    }

    let batch_end = now_iso8601();
    let total_duration = compute_batch_duration(&batch_start, &batch_end);

    success_response(
        json!({
            "rounds_completed": summaries.len(),
            "rounds_requested": rounds_requested,
            "start_round": start_round,
            "final_round_number": last_round_num,
            "stopped_reason": stopped_reason,
            "final_round": final_round_data,
            "rounds_summary": summaries,
            "total_usage": total_usage,
            "total_duration_seconds": total_duration,
        }),
        warnings,
        None,
    )
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
    overrides: RunOverrides,
    warnings: Vec<String>,
) -> Result<Response> {
    let transform = web_sys::TransformStream::new()
        .map_err(|e| Error::JsError(format!("TransformStream::new failed: {e:?}")))?;
    let readable = transform.readable();
    let writable = transform.writable();
    let writer = writable
        .get_writer()
        .map_err(|e| Error::JsError(format!("get_writer failed: {e:?}")))?;
    let writer: web_sys::WritableStreamDefaultWriter = writer.into();

    spawn_local(async move {
        write_sse(&writer, &format_sse_comment("connected")).await;

        if !warnings.is_empty() {
            write_sse(
                &writer,
                &format_sse("warnings", &json!({ "warnings": warnings })),
            )
            .await;
        }

        let mut summaries: Vec<RoundSummary> = Vec::new();
        let mut total_usage = UsageStats {
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
        };
        let mut final_round_data: Option<serde_json::Value> = None;
        let mut stopped_reason = "completed".to_string();
        let mut last_round_num = start_round;

        for i in 0..effective_rounds {
            let round_num = start_round + i;
            last_round_num = round_num;

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

            match execute_round(&kv, &env, &wf, &workflow, round_num, &overrides).await {
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
                            }),
                        ),
                    )
                    .await;

                    final_round_data = Some(round_to_final(round_num, &result));
                    accumulate_usage(&mut total_usage, &result.usage);

                    let converged = stop_on_convergence
                        && summaries.len() + 1 >= min_rounds as usize
                        && result.convergence.score.map_or(false, |s| s >= threshold);

                    summaries.push(summary);

                    if converged {
                        stopped_reason = "convergence".to_string();
                        break;
                    }
                }
                Err(e) => {
                    write_sse(
                        &writer,
                        &format_sse(
                            "error",
                            &json!({
                                "round": round_num,
                                "error": format!("{e:?}"),
                            }),
                        ),
                    )
                    .await;
                    stopped_reason = "error".to_string();
                    break;
                }
            }
        }

        let done_data = json!({
            "rounds_completed": summaries.len(),
            "rounds_requested": rounds_requested,
            "start_round": start_round,
            "final_round_number": last_round_num,
            "stopped_reason": stopped_reason,
            "final_round": final_round_data,
            "rounds_summary": summaries,
            "total_usage": total_usage,
            "total_duration_seconds": 0,
        });
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
