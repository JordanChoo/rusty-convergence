use serde_json::json;
use worker::kv::KvStore;
use worker::*;

use crate::error::{json_error, success_response};
use crate::storage::{kv_get, round_key};
use crate::types::{Round, RoundStatus};
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

    let prompt = format!(
        "The following are revision suggestions from round {round} of iterative specification \
         review for the \"{workflow}\" specification. Apply each suggested change to the \
         specification document, preserving the existing structure where possible. For each \
         change, explain what you modified and why.\n\n---\n\n{content}"
    );

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
