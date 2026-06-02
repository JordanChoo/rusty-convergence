use serde_json::json;
use worker::*;

use crate::error::{success_response, VERSION};

pub async fn handle(env: Env) -> Result<Response> {
    let kv_accessible = env.kv("APRP").is_ok();
    success_response(
        json!({
            "version": VERSION,
            "kv_accessible": kv_accessible,
        }),
        vec![],
        None,
    )
}
