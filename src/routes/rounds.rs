use worker::*;
use worker::kv::KvStore;

use crate::error::json_error;

pub async fn handle_get(
    _kv: KvStore,
    _env: &Env,
    _workflow: &str,
    _round_str: &str,
) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}

pub async fn handle_list(
    _kv: KvStore,
    _env: &Env,
    _workflow: &str,
    _url: &Url,
) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}
