//! Home KV for shared runtime state (Go internal/home/kv_helpers.go). In Home mode
//! caches that Go keeps process-local (Claude device profiles, replay caches, session
//! IDs) live in Home instead, so every CPA node sees the same state.
//!
//! The `*_required` helpers report `home_mode == false` outside Home mode (callers then
//! use their local cache) and an error when Home mode is on but unavailable; the
//! `*_best_effort` ones log that error and degrade to a miss.

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::client::{Client, SetOptions, current};
use crate::error::{Error, Result, redacted_decode_error};

/// Go `HashKeyPart`: hex SHA-256, so keys never carry raw identifiers.
pub fn hash_key_part(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Go `CurrentKVClient`: `Ok(None)` outside Home mode.
pub fn current_client() -> Result<Option<Client>> {
    let Some(client) = current() else {
        return Ok(None);
    };
    if !client.enabled() {
        return Err(Error::Other(format!("home kv store unavailable: {}", Error::Disabled)));
    }
    if !client.heartbeat_ok() {
        return Err(Error::Other(format!(
            "home kv store unavailable: {}",
            Error::NotConnected
        )));
    }
    Ok(Some(client))
}

/// Go `kvLogPrefix`: the first two key segments, never the hashed remainder.
fn log_prefix(key: &str) -> String {
    let key = key.trim();
    if key.is_empty() {
        return "unknown".into();
    }
    let mut parts = key.split(':');
    match (parts.next(), parts.next()) {
        (Some(a), Some(b)) => format!("{a}:{b}:*"),
        (Some(a), None) => format!("{a}:*"),
        _ => "unknown".into(),
    }
}

/// Go `kvSetOptionsForTTL`.
fn ttl_options(ttl: Duration) -> SetOptions {
    SetOptions {
        ex: ttl,
        ..SetOptions::default()
    }
}

/// `(home_mode, value)`.
pub async fn get_json_required<T: DeserializeOwned>(key: &str) -> Result<(bool, Option<T>)> {
    let Some(client) = current_client()? else {
        return Ok((false, None));
    };
    let Some(raw) = client.kv_get(key).await? else {
        return Ok((true, None));
    };
    let value = serde_json::from_slice(&raw).map_err(|e| redacted_decode_error("home kv: decode value", &e))?;
    Ok((true, Some(value)))
}

/// Returns home mode.
pub async fn set_bytes_required(key: &str, value: &[u8], ttl: Duration) -> Result<bool> {
    let Some(client) = current_client()? else {
        return Ok(false);
    };
    if !client.kv_set(key, value, ttl_options(ttl)).await? {
        return Err(Error::Other("home kv store unavailable".into()));
    }
    Ok(true)
}

pub async fn set_json_required<T: Serialize>(key: &str, value: &T, ttl: Duration) -> Result<bool> {
    let raw = serde_json::to_vec(value).map_err(|e| redacted_decode_error("home kv: encode value", &e))?;
    set_bytes_required(key, &raw, ttl).await
}

/// `(home_mode, written)`.
pub async fn set_nx_required(key: &str, value: &[u8], ttl: Duration) -> Result<(bool, bool)> {
    let Some(client) = current_client()? else {
        return Ok((false, false));
    };
    Ok((true, client.kv_set_nx(key, value, ttl).await?))
}

/// `(home_mode, deleted)`.
pub async fn del_required(keys: &[&str]) -> Result<(bool, i64)> {
    let Some(client) = current_client()? else {
        return Ok((false, 0));
    };
    Ok((true, client.kv_del(keys).await?))
}

pub async fn expire_required(key: &str, ttl: Duration) -> Result<bool> {
    let Some(client) = current_client()? else {
        return Ok(false);
    };
    client.kv_expire(key, ttl).await?;
    Ok(true)
}

/// `(home_mode, value)`; errors are logged and read as a miss.
pub async fn get_json_best_effort<T: DeserializeOwned>(key: &str) -> (bool, Option<T>) {
    match get_json_required(key).await {
        Ok(found) => found,
        Err(error) => {
            tracing::error!("home kv best-effort get failed prefix={}: {error}", log_prefix(key));
            (true, None)
        }
    }
}

pub async fn set_json_best_effort<T: Serialize>(key: &str, value: &T, ttl: Duration) -> bool {
    match serde_json::to_vec(value) {
        Ok(raw) => set_bytes_best_effort(key, &raw, ttl).await,
        Err(error) => {
            tracing::error!("home kv best-effort set failed prefix={}: {error}", log_prefix(key));
            false
        }
    }
}

pub async fn set_bytes_best_effort(key: &str, value: &[u8], ttl: Duration) -> bool {
    match set_bytes_required(key, value, ttl).await {
        Ok(home_mode) => home_mode,
        Err(error) => {
            tracing::error!("home kv best-effort set failed prefix={}: {error}", log_prefix(key));
            false
        }
    }
}

pub async fn set_nx_best_effort(key: &str, value: &[u8], ttl: Duration) -> bool {
    match set_nx_required(key, value, ttl).await {
        Ok((_, written)) => written,
        Err(error) => {
            tracing::error!("home kv best-effort setnx failed prefix={}: {error}", log_prefix(key));
            false
        }
    }
}

pub async fn del_best_effort(keys: &[&str]) -> bool {
    match del_required(keys).await {
        Ok((home_mode, _)) => home_mode,
        Err(error) => {
            let first = keys.first().copied().unwrap_or_default();
            tracing::error!("home kv best-effort del failed prefix={}: {error}", log_prefix(first));
            false
        }
    }
}

pub async fn expire_best_effort(key: &str, ttl: Duration) -> bool {
    match expire_required(key, ttl).await {
        Ok(home_mode) => home_mode,
        Err(error) => {
            tracing::error!("home kv best-effort expire failed prefix={}: {error}", log_prefix(key));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_log_prefix_match_go() {
        // Go TestHashKeyPart: sha256("abc").
        assert_eq!(
            hash_key_part("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(log_prefix("cpa:claude:device:abc"), "cpa:claude:*");
        assert_eq!(log_prefix("single"), "single:*");
        assert_eq!(log_prefix("  "), "unknown");
    }
}
