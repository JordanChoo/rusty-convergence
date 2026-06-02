use worker::*;
use worker::kv::KvStore;

use crate::error::json_error;

pub async fn handle(_kv: KvStore, _workflow: &str, _round_str: &str) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}
