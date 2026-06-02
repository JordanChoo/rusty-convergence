use worker::*;
use worker::kv::KvStore;

use crate::error::json_error;

pub async fn handle_get(_kv: KvStore, _workflow: &str) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}

pub async fn handle_rebuild(_kv: KvStore, _workflow: &str) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}
