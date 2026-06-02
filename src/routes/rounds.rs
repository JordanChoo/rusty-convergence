use serde_json::json;
use worker::kv::KvStore;
use worker::*;

use crate::error::{json_error, success_response};
use crate::routes::run::{default_lock_ttl, effective_status};
use crate::storage::{config_key, kv_get, kv_list_by_prefix, round_key};
use crate::types::{Round, RoundStatus, Workflow};
use crate::validation::parse_and_validate_round;

pub async fn handle_get(
    kv: KvStore,
    env: &Env,
    workflow: &str,
    round_str: &str,
) -> Result<Response> {
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

    let lock_ttl = default_lock_ttl(env);
    let status = effective_status(&record, lock_ttl);

    match status {
        RoundStatus::Running => success_response(
            json!({
                "workflow": workflow,
                "round": round,
                "status": "running",
                "started_at": record.started_at,
            }),
            vec![],
            None,
        ),
        RoundStatus::Stale => {
            let elapsed = elapsed_minutes(&record.started_at);
            success_response(
                json!({
                    "workflow": workflow,
                    "round": round,
                    "status": "stale",
                    "started_at": record.started_at,
                    "stale_reason": format!(
                        "Round has been running for {elapsed} minutes (lock TTL: {} minutes). The stream likely disconnected. Retry with POST /run/{workflow}/{round}.",
                        lock_ttl / 60
                    ),
                }),
                vec![],
                None,
            )
        }
        RoundStatus::Complete => success_response(
            json!({
                "workflow": workflow,
                "round": round,
                "status": "complete",
                "content": record.content,
                "metrics": record.metrics,
                "convergence": record.convergence,
                "usage": record.usage,
                "provider": record.provider,
                "model": record.model,
                "started_at": record.started_at,
                "completed_at": record.completed_at,
                "duration_seconds": record.duration_seconds,
            }),
            vec![],
            None,
        ),
        RoundStatus::Failed => success_response(
            json!({
                "workflow": workflow,
                "round": round,
                "status": "failed",
                "error": record.error,
                "partial_content": record.partial_content,
                "partial_bytes": record.partial_content.as_ref().map(|c| c.len()),
                "started_at": record.started_at,
                "failed_at": record.failed_at,
            }),
            vec![],
            None,
        ),
    }
}

pub async fn handle_list(
    kv: KvStore,
    env: &Env,
    workflow: &str,
    url: &Url,
) -> Result<Response> {
    if kv_get::<Workflow>(&kv, &config_key(workflow))
        .await?
        .is_none()
    {
        return json_error(
            404,
            &format!("Workflow '{workflow}' does not exist"),
            "not_found",
            Some("Use POST /workflows to create a workflow first"),
        );
    }

    let params: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    let status_filter = params.get("status");
    let limit: u64 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
        .min(100);
    let cursor = params.get("cursor").map(|s| s.as_str());

    let prefix = format!("round::{workflow}::");
    let (keys, next_cursor) = kv_list_by_prefix(&kv, &prefix, limit, cursor).await?;

    let lock_ttl = default_lock_ttl(env);
    let mut rounds = Vec::new();

    for key in &keys {
        if let Some(record) = kv_get::<Round>(&kv, key).await? {
            let status = effective_status(&record, lock_ttl);

            if let Some(filter) = status_filter {
                let status_str = serde_json::to_value(&status)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if &status_str != filter {
                    continue;
                }
            }

            let convergence_score = record.convergence.as_ref().and_then(|c| c.score);
            rounds.push(json!({
                "round": record.round,
                "status": status,
                "words": record.metrics.as_ref().map(|m| m.words),
                "convergence_score": convergence_score,
                "completed_at": record.completed_at,
            }));
        }
    }

    success_response(
        json!({
            "workflow": workflow,
            "rounds": rounds,
            "cursor": next_cursor,
        }),
        vec![],
        None,
    )
}

fn elapsed_minutes(started_at: &str) -> u64 {
    let Ok(start) = chrono::DateTime::parse_from_rfc3339(started_at) else {
        return 0;
    };
    let now_millis = Date::now().as_millis();
    let now_secs = (now_millis / 1000) as i64;
    let elapsed_secs = now_secs - start.timestamp();
    if elapsed_secs > 0 {
        elapsed_secs as u64 / 60
    } else {
        0
    }
}
