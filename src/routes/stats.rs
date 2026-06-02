use serde_json::json;
use worker::kv::KvStore;
use worker::*;

use crate::convergence::{
    compute_change_velocity, compute_output_trend, read_stats, rebuild_stats_from_rounds,
};
use crate::error::{json_error, now_iso8601, success_response};
use crate::storage::{config_key, kv_get, kv_list_by_prefix, kv_put, meta_key};
use crate::types::Meta;
use crate::types::{Round, RoundStatus, Workflow};

pub async fn handle_get(kv: KvStore, workflow: &str) -> Result<Response> {
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

    match read_stats(&kv, workflow).await? {
        Some(stats) => {
            let rounds_json: Vec<serde_json::Value> = stats
                .rounds
                .iter()
                .map(|e| {
                    json!({
                        "round": e.round,
                        "words": e.words,
                        "delta_words": e.delta_words,
                        "similarity": e.similarity,
                        "score": e.score,
                    })
                })
                .collect();

            let convergence = if let Some(score) = stats.latest_score {
                let word_counts: Vec<u32> = stats.rounds.iter().map(|e| e.words).collect();
                let output_trend = compute_output_trend(&word_counts);
                let change_velocity = compute_change_velocity(&word_counts);
                let last = stats.rounds.last();
                json!({
                    "score": score,
                    "output_trend": output_trend,
                    "change_velocity": change_velocity,
                    "similarity_trend": last.and_then(|e| e.similarity),
                    "estimated_remaining_rounds": estimated_remaining(score),
                    "recommendation": recommendation(score),
                })
            } else {
                json!(null)
            };

            success_response(
                json!({
                    "workflow": workflow,
                    "total_rounds": stats.total_rounds,
                    "convergence": convergence,
                    "rounds": rounds_json,
                }),
                vec![],
                None,
            )
        }
        None => success_response(
            json!({
                "workflow": workflow,
                "total_rounds": 0,
                "convergence": null,
                "rounds": [],
            }),
            vec![],
            None,
        ),
    }
}

pub async fn handle_rebuild(kv: KvStore, workflow: &str) -> Result<Response> {
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

    let prefix = format!("round::{workflow}::");
    let mut completed_rounds: Vec<(u32, String, u32)> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (keys, next) = kv_list_by_prefix(&kv, &prefix, 100, cursor.as_deref()).await?;
        for key in &keys {
            if let Some(round) = kv_get::<Round>(&kv, key).await? {
                if round.status == RoundStatus::Complete {
                    let content = round.content.unwrap_or_default();
                    let words = content.split_whitespace().count() as u32;
                    completed_rounds.push((round.round, content, words));
                }
            }
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }

    completed_rounds.sort_by_key(|(r, _, _)| *r);

    let stats = rebuild_stats_from_rounds(&kv, workflow, &completed_rounds).await?;

    let now = now_iso8601();
    let existing_meta = kv_get::<Meta>(&kv, &meta_key(workflow)).await?;
    let meta = Meta {
        workflow: workflow.to_string(),
        round_count: stats.total_rounds,
        latest_round: completed_rounds.last().map(|(r, _, _)| *r),
        latest_convergence: stats.latest_score,
        created_at: existing_meta
            .as_ref()
            .map(|m| m.created_at.clone())
            .unwrap_or_else(|| now.clone()),
        updated_at: now,
    };
    kv_put(&kv, &meta_key(workflow), &meta).await?;

    success_response(
        json!({
            "workflow": workflow,
            "rounds_processed": stats.total_rounds,
            "convergence_score": stats.latest_score,
        }),
        vec![],
        None,
    )
}

fn recommendation(score: f64) -> &'static str {
    if score >= 0.90 {
        "stop"
    } else if score >= 0.75 {
        "almost"
    } else if score >= 0.50 {
        "continue"
    } else {
        "early"
    }
}

fn estimated_remaining(score: f64) -> &'static str {
    if score >= 0.90 {
        "0"
    } else if score >= 0.75 {
        "1-2"
    } else if score >= 0.50 {
        "3-5"
    } else {
        "5+"
    }
}
