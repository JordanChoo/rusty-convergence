use worker::*;
use worker::kv::KvStore;

use crate::error::json_error;

pub async fn handle_list(_kv: KvStore, _url: &Url) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}

pub async fn handle_create(_kv: KvStore, _req: Request) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}

pub async fn handle_get(_kv: KvStore, _name: &str) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}

pub async fn handle_delete(_kv: KvStore, _name: &str) -> Result<Response> {
    json_error(501, "Not implemented", "not_implemented", None)
}
