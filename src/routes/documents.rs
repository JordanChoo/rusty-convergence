use worker::*;
use worker::kv::KvStore;

use crate::error::json_error;

pub async fn handle_upload(
    _kv: KvStore,
    _workflow: &str,
    _role: &str,
    _req: Request,
) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}

pub async fn handle_get(_kv: KvStore, _workflow: &str, _role: &str) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}
