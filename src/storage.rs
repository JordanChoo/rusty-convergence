use serde::de::DeserializeOwned;
use serde::Serialize;
use worker::kv::KvStore;
use worker::*;

pub fn config_key(workflow: &str) -> String {
    format!("config::{workflow}")
}

pub fn doc_key(workflow: &str, role: &str) -> String {
    format!("doc::{workflow}::{role}")
}

pub fn round_key(workflow: &str, round: u32) -> String {
    format!("round::{workflow}::{round}")
}

pub fn meta_key(workflow: &str) -> String {
    format!("meta::{workflow}")
}

pub fn stats_key(workflow: &str) -> String {
    format!("stats::{workflow}")
}

pub fn lock_key(workflow: &str) -> String {
    format!("lock::{workflow}")
}

pub fn parse_round_number_from_key(key: &str) -> Option<u32> {
    key.rsplit("::").next()?.parse().ok()
}

pub async fn kv_get<T: DeserializeOwned>(kv: &KvStore, key: &str) -> Result<Option<T>> {
    match kv.get(key).text().await.map_err(|e| Error::RustError(e.to_string()))? {
        Some(text) => {
            let val: T = serde_json::from_str(&text)
                .map_err(|e| Error::RustError(format!("KV deserialize error for {key}: {e}")))?;
            Ok(Some(val))
        }
        None => Ok(None),
    }
}

pub async fn kv_get_text(kv: &KvStore, key: &str) -> Result<Option<String>> {
    kv.get(key).text().await.map_err(|e| Error::RustError(e.to_string()))
}

pub async fn kv_put<T: Serialize>(kv: &KvStore, key: &str, value: &T) -> Result<()> {
    let json = serde_json::to_string(value)
        .map_err(|e| Error::RustError(format!("KV serialize error for {key}: {e}")))?;
    kv.put(key, json)
        .map_err(|e| Error::RustError(e.to_string()))?
        .execute()
        .await
        .map_err(|e| Error::RustError(e.to_string()))
}

pub async fn kv_put_text(kv: &KvStore, key: &str, text: &str) -> Result<()> {
    kv.put(key, text)
        .map_err(|e| Error::RustError(e.to_string()))?
        .execute()
        .await
        .map_err(|e| Error::RustError(e.to_string()))
}

pub async fn kv_put_with_ttl<T: Serialize>(
    kv: &KvStore,
    key: &str,
    value: &T,
    ttl_seconds: u64,
) -> Result<()> {
    let json = serde_json::to_string(value)
        .map_err(|e| Error::RustError(format!("KV serialize error for {key}: {e}")))?;
    kv.put(key, json)
        .map_err(|e| Error::RustError(e.to_string()))?
        .expiration_ttl(ttl_seconds)
        .execute()
        .await
        .map_err(|e| Error::RustError(e.to_string()))
}

pub async fn kv_delete(kv: &KvStore, key: &str) -> Result<()> {
    kv.delete(key).await.map_err(|e| Error::RustError(e.to_string()))
}

pub async fn kv_list_by_prefix(
    kv: &KvStore,
    prefix: &str,
    limit: u64,
    cursor: Option<&str>,
) -> Result<(Vec<String>, Option<String>)> {
    let mut builder = kv.list().prefix(prefix.to_string()).limit(limit);
    if let Some(c) = cursor {
        builder = builder.cursor(c.to_string());
    }
    let result = builder.execute().await.map_err(|e| Error::RustError(e.to_string()))?;
    let keys: Vec<String> = result.keys.into_iter().map(|k| k.name).collect();
    let next_cursor = if result.list_complete {
        None
    } else {
        result.cursor
    };
    Ok((keys, next_cursor))
}

use crate::error::now_iso8601;
use crate::types::Lock;

pub async fn acquire_lock(
    kv: &KvStore,
    workflow: &str,
    round: u32,
    ttl_seconds: u64,
) -> Result<std::result::Result<(), Lock>> {
    let key = lock_key(workflow);
    if let Some(existing) = kv_get::<Lock>(kv, &key).await? {
        return Ok(Err(existing));
    }
    let now = now_iso8601();
    let lock = Lock {
        round,
        started_at: now.clone(),
        expires_at: format_expiry(&now, ttl_seconds),
    };
    kv_put_with_ttl(kv, &key, &lock, ttl_seconds).await?;
    Ok(Ok(()))
}

pub async fn release_lock(kv: &KvStore, workflow: &str) -> Result<()> {
    kv_delete(kv, &lock_key(workflow)).await
}

pub async fn check_lock(kv: &KvStore, workflow: &str) -> Result<Option<Lock>> {
    kv_get::<Lock>(kv, &lock_key(workflow)).await
}

fn format_expiry(started_at: &str, ttl_seconds: u64) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(started_at) {
        let expiry = dt + chrono::Duration::seconds(ttl_seconds as i64);
        expiry.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    } else {
        "unknown".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_key() {
        assert_eq!(config_key("fcp-spec"), "config::fcp-spec");
    }

    #[test]
    fn test_doc_key() {
        assert_eq!(doc_key("fcp-spec", "readme"), "doc::fcp-spec::readme");
    }

    #[test]
    fn test_round_key_unpadded() {
        assert_eq!(round_key("fcp-spec", 5), "round::fcp-spec::5");
        assert_eq!(round_key("fcp-spec", 999), "round::fcp-spec::999");
    }

    #[test]
    fn test_meta_key() {
        assert_eq!(meta_key("fcp-spec"), "meta::fcp-spec");
    }

    #[test]
    fn test_stats_key() {
        assert_eq!(stats_key("fcp-spec"), "stats::fcp-spec");
    }

    #[test]
    fn test_lock_key() {
        assert_eq!(lock_key("fcp-spec"), "lock::fcp-spec");
    }

    #[test]
    fn test_parse_round_number_valid() {
        assert_eq!(parse_round_number_from_key("round::fcp-spec::5"), Some(5));
        assert_eq!(
            parse_round_number_from_key("round::fcp-spec::999"),
            Some(999)
        );
    }

    #[test]
    fn test_parse_round_number_invalid() {
        assert_eq!(parse_round_number_from_key("config::fcp-spec"), None);
        assert_eq!(parse_round_number_from_key("round::fcp-spec::abc"), None);
        assert_eq!(parse_round_number_from_key(""), None);
    }
}
