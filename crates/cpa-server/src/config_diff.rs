//! Redacted reload summaries (Go internal/watcher/diff). Only explicitly
//! allowlisted scalar values are printed; payloads, keys and headers are not.
use std::collections::{BTreeMap, BTreeSet};

use cpa_core::config::Config;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn get<'a>(value: &'a Value, path: &str) -> &'a Value {
    path.split('.').fold(value, |v, k| &v[k])
}
fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}
fn trim(v: &Value) -> &str {
    crate::gojson::trim(text(v))
}
fn lower(s: &str) -> String {
    cpa_common::gostr::lower_bytes(s.as_bytes())
}
fn list(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or_default()
}
fn number(v: &Value) -> i64 {
    v.as_i64().unwrap_or_default()
}
fn boolean(v: &Value) -> bool {
    v.as_bool().unwrap_or_default()
}
fn hash(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn url(v: &Value) -> String {
    let raw = trim(v);
    if raw.is_empty() {
        return "<none>".into();
    }
    let Some(parsed) = cpa_core::config::go_url::parse(raw) else {
        return "<redacted>".into();
    };
    if parsed.host().is_empty() {
        return cpa_core::config::go_url::parse(&format!("http://{raw}"))
            .filter(|p| !p.host().is_empty())
            .map(|p| p.host().to_owned())
            .unwrap_or_else(|| "<redacted>".into());
    }
    if parsed.scheme.is_empty() {
        parsed.host().to_owned()
    } else {
        format!("{}://{}", parsed.scheme, parsed.host())
    }
}

fn scalar(v: &Value, kind: char) -> String {
    match kind {
        'b' => boolean(v).to_string(),
        'i' => number(v).to_string(),
        't' => trim(v).to_owned(),
        'u' => url(v),
        'q' => crate::gojson::string(text(v)),
        'o' if v.is_null() => "inherit".into(),
        'p' if v.is_null() => "<unset>".into(),
        'n' if v.is_null() => "<nil>".into(),
        'o' | 'p' | 'n' => v.to_string(),
        'e' if trim(v).is_empty() => "<none>".into(),
        'e' => trim(v).to_owned(),
        'm' if v.is_string() => text(v).to_owned(),
        'm' => boolean(v).to_string(),
        _ => text(v).to_owned(),
    }
}

fn changed(out: &mut Vec<String>, label: &str, old: &Value, new: &Value, kind: char) {
    let a = scalar(old, kind);
    let b = scalar(new, kind);
    // URLs compare the original trimmed strings, even when their redacted
    // printable host is unchanged (Go formatURL is presentation only).
    let differs = if kind == 'u' { trim(old) != trim(new) } else { a != b };
    if differs {
        out.push(format!("{label}: {a} -> {b}"));
    }
}

fn excluded(v: &Value) -> Vec<String> {
    list(v)
        .iter()
        .map(|v| lower(trim(v)))
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn model_summary(v: &Value, family: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for model in list(v) {
        let (name, alias) = (trim(&model["name"]), trim(&model["alias"]));
        if name.is_empty() && alias.is_empty() {
            continue;
        }
        let mut thinking = model["thinking"].clone();
        if let Some(fields) = thinking.as_object_mut() {
            fields.retain(|_, value| {
                !value.is_null() && value != false && value != 0 && !value.as_array().is_some_and(Vec::is_empty)
            });
        }
        let thinking = crate::gojson::sorted(&thinking);
        let key = if family == "vertex" {
            format!(
                "{}|{}|thinking={thinking}",
                if alias.is_empty() { name } else { alias },
                trim(&model["display-name"])
            )
        } else {
            format!(
                "{}|{}|{}{}|is-compat={}|thinking={thinking}",
                lower(name),
                lower(alias),
                trim(&model["display-name"]),
                if matches!(family, "codex" | "xai" | "meta") {
                    format!("|force-mapping={}", boolean(&model["force-mapping"]))
                } else {
                    String::new()
                },
                boolean(&model["is-compat"])
            )
        };
        keys.push(key);
    }
    keys.sort();
    if family != "vertex" {
        keys.dedup();
    }
    keys
}

fn map_summary(v: &Value, section: &str) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for (channel, entries) in v.as_object().into_iter().flatten() {
        let channel = lower(crate::gojson::trim(channel));
        if channel.is_empty() {
            continue;
        }
        let mut keys = Vec::new();
        if section == "oauth-excluded-models" {
            keys = excluded(entries);
        } else {
            for entry in list(entries) {
                let name = lower(trim(&entry["name"]));
                let alias = lower(trim(&entry["alias"]));
                let key = match section {
                    "oauth-model-alias" if !name.is_empty() && !alias.is_empty() => format!(
                        "{name}->{alias}{}{}{}",
                        if boolean(&entry["fork"]) { "|fork" } else { "" },
                        if trim(&entry["display-name"]).is_empty() {
                            String::new()
                        } else {
                            format!("|display-name={}", trim(&entry["display-name"]))
                        },
                        if boolean(&entry["force-mapping"]) {
                            "|force-mapping"
                        } else {
                            ""
                        }
                    ),
                    "oauth-settings" if !name.is_empty() => format!(
                        "{name}->{alias}{}",
                        if number(&entry["max-context-length"]) > 0 {
                            format!("|max-context-length={}", number(&entry["max-context-length"]))
                        } else {
                            String::new()
                        }
                    ),
                    "oauth-request-scoped-errors"
                        if number(&entry["status"]) > 0
                            && (!list(&entry["match"]).is_empty() || !list(&entry["match-regexr"]).is_empty())
                            && !text(&entry["action"]).is_empty() =>
                    {
                        format!(
                            "{}|{}|{}|{}",
                            number(&entry["status"]),
                            list(&entry["match"]).iter().map(text).collect::<Vec<_>>().join(","),
                            list(&entry["match-regexr"])
                                .iter()
                                .map(text)
                                .collect::<Vec<_>>()
                                .join(","),
                            text(&entry["action"])
                        )
                    }
                    _ => continue,
                };
                if section == "oauth-request-scoped-errors" || !keys.contains(&key) {
                    keys.push(key);
                }
            }
            if section == "oauth-model-alias" {
                keys.sort();
            }
        }
        out.insert(channel, keys);
    }
    out
}

fn oauth_changes(out: &mut Vec<String>, old: &Value, new: &Value, section: &str) {
    let (old, new) = (map_summary(&old[section], section), map_summary(&new[section], section));
    let channels: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    let mut changes = Vec::new();
    for channel in channels {
        let label = format!("{section}[{channel}]");
        match (old.get(channel), new.get(channel)) {
            (Some(_), None) => changes.push(format!("{label}: removed")),
            (None, Some(n)) => changes.push(format!("{label}: added ({} entries)", n.len())),
            (Some(o), Some(n)) if o != n => {
                changes.push(format!("{label}: updated ({} -> {} entries)", o.len(), n.len()))
            }
            _ => {}
        }
    }
    changes.sort();
    out.extend(changes);
}

fn compat_keys(entries: &Value) -> BTreeMap<String, (String, &Value)> {
    let mut out = BTreeMap::new();
    for (i, entry) in list(entries).iter().enumerate() {
        let (base, label) = if !trim(&entry["name"]).is_empty() {
            (
                format!("name:{}", trim(&entry["name"])),
                trim(&entry["name"]).to_owned(),
            )
        } else if !trim(&entry["base-url"]).is_empty() {
            (format!("base:{}", trim(&entry["base-url"])), url(&entry["base-url"]))
        } else if let Some(alias) = list(&entry["models"])
            .iter()
            .map(|m| {
                if trim(&m["alias"]).is_empty() {
                    trim(&m["name"])
                } else {
                    trim(&m["alias"])
                }
            })
            .find(|s| !s.is_empty())
        {
            (format!("alias:{alias}"), alias.to_owned())
        } else {
            let mut parts = Vec::new();
            let mut models: Vec<_> = list(&entry["models"])
                .iter()
                .filter(|m| !trim(&m["name"]).is_empty() || !trim(&m["alias"]).is_empty())
                .map(|m| {
                    format!(
                        "{}|{}|{}|image={}",
                        lower(trim(&m["name"])),
                        lower(trim(&m["alias"])),
                        trim(&m["display-name"]),
                        boolean(&m["image"])
                    )
                })
                .collect();
            models.sort();
            if !models.is_empty() {
                parts.push(format!("models={}", models.join(",")));
            }
            let mut headers: Vec<_> = entry["headers"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| lower(crate::gojson::trim(k)))
                .filter(|s| !s.is_empty())
                .collect();
            headers.sort();
            if !headers.is_empty() {
                parts.push(format!("headers={}", headers.join(",")));
            }
            let count = compat_key_count(entry);
            if count > 0 {
                parts.push(format!("api_keys={count}"));
            }
            if parts.is_empty() {
                (format!("index:{i}"), format!("entry-{}", i + 1))
            } else {
                let digest = hash(&parts.join("|"));
                (format!("sig:{digest}"), format!("compat-{}", &digest[..8]))
            }
        };
        let mut key = base.clone();
        let mut duplicate = 1;
        while out.contains_key(&key) {
            key = format!("duplicate:{base}:{duplicate}");
            duplicate += 1;
        }
        out.insert(key, (label, entry));
    }
    out
}
fn compat_key_count(v: &Value) -> usize {
    list(&v["api-key-entries"])
        .iter()
        .filter(|e| !trim(&e["api-key"]).is_empty())
        .count()
}
fn compat_model_count(v: &Value) -> usize {
    list(&v["models"])
        .iter()
        .filter(|m| !trim(&m["name"]).is_empty() || !trim(&m["alias"]).is_empty())
        .count()
}

fn compat_changes(out: &mut Vec<String>, old: &Value, new: &Value) {
    let (old, new) = (
        compat_keys(&old["openai-compatibility"]),
        compat_keys(&new["openai-compatibility"]),
    );
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    let mut changes = Vec::new();
    for key in keys {
        match (old.get(key), new.get(key)) {
            (None, Some((label, n))) => changes.push(format!(
                "provider added: {label} (api-keys={}, models={})",
                compat_key_count(n),
                compat_model_count(n)
            )),
            (Some((label, o)), None) => changes.push(format!(
                "provider removed: {label} (api-keys={}, models={})",
                compat_key_count(o),
                compat_model_count(o)
            )),
            (Some((label, o)), Some((_, n))) => {
                let mut details = Vec::new();
                for (field, kind) in [
                    ("disabled", 'b'),
                    ("support-prompt-cache-key", 'b'),
                    ("disable-cooling", 'o'),
                    ("request-retry", 'p'),
                ] {
                    let a = scalar(&o[field], kind);
                    let b = scalar(&n[field], kind);
                    if a != b {
                        details.push(format!("{field} {a} -> {b}"));
                    }
                }
                for (field, count) in [
                    ("api-keys", compat_key_count as fn(&Value) -> usize),
                    ("models", compat_model_count),
                ] {
                    if count(o) != count(n) {
                        details.push(format!("{field} {} -> {}", count(o), count(n)));
                    }
                }
                if o["headers"] != n["headers"] {
                    details.push("headers updated".into());
                }
                if !details.is_empty() {
                    changes.push(format!("provider updated: {label} ({})", details.join(", ")));
                }
            }
            _ => {}
        }
    }
    if !changes.is_empty() {
        out.push("openai-compatibility:".into());
        out.extend(changes.into_iter().map(|s| format!("  {s}")));
    }
}

pub(crate) fn details(old_cfg: &Config, new_cfg: &Config) -> Vec<String> {
    let view = |cfg: &Config, snapshot| {
        let mut view = crate::management::legacy::view::config_for_diff(&cfg.document, snapshot);
        view["port"] = cfg.port.into();
        view["auth-dir"] = cfg.auth_dir.to_string_lossy().into_owned().into();
        let m = &cfg.management;
        view["remote-management"] = serde_json::json!({
            "allow-remote": m.allow_remote,
            "disable-control-panel": m.disable_control_panel,
            "disable-auto-update-panel": m.disable_auto_update_panel,
            "panel-github-repository": if crate::gojson::trim(&m.panel_github_repository).is_empty() { "https://github.com/router-for-me/Cli-Proxy-API-Management-Center" } else { crate::gojson::trim(&m.panel_github_repository) },
            "base-url": cfg.document.get("management").and_then(|m| m.get("base-url")).and_then(serde_yaml_ng::Value::as_str).unwrap_or_default(),
        });
        view
    };
    let (old, new) = (view(old_cfg, true), view(new_cfg, false));
    let mut out = Vec::new();
    for (path, kind) in [
        ("client.codex.enable-apply-patch", 'b'),
        ("port", 'i'),
        ("auth-dir", 's'),
        ("debug", 'b'),
        ("pprof.enable", 'b'),
        ("pprof.addr", 't'),
        ("logging-to-file", 'b'),
        ("usage-statistics-enabled", 'b'),
        ("redis-usage-queue-retention-seconds", 'i'),
        ("disable-cooling", 'b'),
        ("save-cooldown-status", 'b'),
        ("transient-error-cooldown-seconds", 'i'),
        ("disable-claude-cloak-mode", 'b'),
        ("claude-code.disable-cloaking-model-list", 'b'),
        ("disable-image-generation", 'm'),
        ("gpt-image-2-base-model", 't'),
        ("request-log", 'b'),
        ("logs-max-total-size-mb", 'i'),
        ("error-logs-max-files", 'i'),
        ("request-retry", 'i'),
        ("max-retry-credentials", 'i'),
        ("max-retry-interval", 'i'),
        ("proxy-url", 'u'),
        ("ws-auth", 'b'),
        ("force-model-prefix", 'b'),
        ("nonstream-keepalive-interval", 'i'),
        ("quota-exceeded.switch-project", 'b'),
        ("quota-exceeded.switch-preview-model", 'b'),
        ("quota-exceeded.antigravity-credits", 'b'),
    ] {
        changed(&mut out, path, get(&old, path), get(&new, path), kind);
    }
    for path in ["antigravity.sensitive-words", "devin.sensitive-words"] {
        if get(&old, path) != get(&new, path) {
            out.push(format!(
                "{path}: {} -> {}",
                list(get(&old, path)).len(),
                list(get(&new, path)).len()
            ));
        }
    }
    for (path, kind) in [
        ("antigravity.connection-pool.enabled", 'n'),
        ("antigravity.connection-pool.idle-conn-timeout", 'q'),
        ("antigravity.connection-pool.max-idle-conns-per-host", 'n'),
        ("codex.disable-codex-cloaking", 'b'),
        ("codex.stream-bootstrap-buffering", 'b'),
        ("codex.stream-bootstrap-timeout", 't'),
        ("client.codex.optimize-multi-agent-v2", 'b'),
        ("codex.orphan-delegation-compatibility", 'b'),
        ("xai.inject-x-search", 'b'),
        ("codex.live-media-relay.enabled", 'b'),
        ("codex.live-media-relay.max-sessions", 'i'),
        ("codex.live-media-relay.disable-private-remote-ips", 'b'),
        ("codex.live-media-relay.public-ip", 'e'),
        ("codex.live-media-relay.udp-port-min", 'i'),
        ("codex.live-media-relay.udp-port-max", 'i'),
    ] {
        changed(&mut out, path, get(&old, path), get(&new, path), kind);
    }
    let ice = "codex.live-media-relay.ice-servers";
    if get(&old, ice) != get(&new, ice) {
        out.push(format!(
            "{ice}: updated ({} -> {} entries, credentials redacted)",
            list(get(&old, ice)).len(),
            list(get(&new, ice)).len()
        ));
    }
    changed(
        &mut out,
        "routing.strategy",
        get(&old, "routing.strategy"),
        get(&new, "routing.strategy"),
        's',
    );
    for section in ["default", "default-raw", "override", "override-raw", "filter"] {
        let (o, n) = (&old["payload"][section], &new["payload"][section]);
        if o != n {
            out.push(format!(
                "payload.{section}: updated ({} -> {} rules)",
                list(o).len(),
                list(n).len()
            ));
        }
    }
    let old_keys = list(&old["api-keys"]);
    let new_keys = list(&new["api-keys"]);
    if old_keys.len() != new_keys.len() {
        out.push(format!("api-keys count: {} -> {}", old_keys.len(), new_keys.len()));
    } else if old_keys.iter().map(trim).ne(new_keys.iter().map(trim)) {
        out.push("api-keys: values updated (count unchanged, redacted)".into());
    }
    for family in ["gemini", "interactions", "claude", "codex", "xai", "meta"] {
        provider_changes(&mut out, &old, &new, family);
    }
    for section in [
        "oauth-excluded-models",
        "oauth-model-alias",
        "oauth-request-scoped-errors",
        "oauth-settings",
    ] {
        oauth_changes(&mut out, &old, &new, section);
    }
    for (path, kind) in [
        ("remote-management.allow-remote", 'b'),
        ("remote-management.disable-control-panel", 'b'),
        ("remote-management.disable-auto-update-panel", 'b'),
        ("remote-management.panel-github-repository", 'u'),
        ("remote-management.base-url", 'u'),
    ] {
        changed(&mut out, path, get(&old, path), get(&new, path), kind);
    }
    if old_cfg.management.secret_key != new_cfg.management.secret_key {
        out.push(format!(
            "remote-management.secret-key: {}",
            if old_cfg.management.secret_key.is_empty() {
                "created"
            } else if new_cfg.management.secret_key.is_empty() {
                "deleted"
            } else {
                "updated"
            }
        ));
    }
    compat_changes(&mut out, &old, &new);
    provider_changes(&mut out, &old, &new, "vertex");
    out
}

pub(crate) fn log(old: &Config, new: &Config) {
    let debug = |cfg: &Config| {
        cfg.document
            .get("observability")
            .and_then(|v| v.get("logs"))
            .and_then(|v| v.get("debug"))
            .and_then(serde_yaml_ng::Value::as_bool)
            .unwrap_or(false)
    };
    let old_debug = debug(old);
    let new_debug = debug(new);
    if old_debug != new_debug {
        tracing::debug!("log level updated - debug mode changed from {old_debug} to {new_debug}");
    }
    let changes = details(old, new);
    if changes.is_empty() {
        tracing::debug!("no material config field changes detected");
    } else {
        tracing::info!("config changes detected:");
        for change in changes {
            tracing::info!("  {change}");
        }
    }
}

fn provider_changes(out: &mut Vec<String>, old: &Value, new: &Value, family: &str) {
    let section = format!("{family}-api-key");
    let (old, new) = (list(&old[&section]), list(&new[&section]));
    if old.len() != new.len() {
        out.push(format!("{section} count: {} -> {}", old.len(), new.len()));
        return;
    }
    for (i, (o, n)) in old.iter().zip(new).enumerate() {
        let prefix = format!("{family}[{i}]");
        for field in ["base-url", "proxy-url", "prefix"] {
            changed(
                out,
                &format!("{prefix}.{field}"),
                &o[field],
                &n[field],
                if field == "prefix" { 't' } else { 'u' },
            );
        }
        if family == "codex" {
            for field in ["websockets", "alpha-search"] {
                changed(out, &format!("{prefix}.{field}"), &o[field], &n[field], 'b');
            }
        }
        if matches!(family, "xai" | "meta") {
            changed(out, &format!("{prefix}.priority"), &o["priority"], &n["priority"], 'i');
        }
        if family == "xai" {
            changed(
                out,
                &format!("{prefix}.websockets"),
                &o["websockets"],
                &n["websockets"],
                'b',
            );
        }
        changed(
            out,
            &format!("{prefix}.disable-cooling"),
            &o["disable-cooling"],
            &n["disable-cooling"],
            'o',
        );
        if family == "codex" {
            changed(
                out,
                &format!("{prefix}.disable-codex-cloaking"),
                &o["disable-codex-cloaking"],
                &n["disable-codex-cloaking"],
                'o',
            );
        }
        if matches!(family, "xai" | "meta") {
            changed(
                out,
                &format!("{prefix}.request-retry"),
                &o["request-retry"],
                &n["request-retry"],
                'p',
            );
        }
        if trim(&o["api-key"]) != trim(&n["api-key"]) {
            out.push(format!("{prefix}.api-key: updated"));
        }
        if family != "vertex" && o["headers"] != n["headers"] {
            out.push(format!("{prefix}.headers: updated"));
        }
        let (a, b) = (model_summary(&o["models"], family), model_summary(&n["models"], family));
        if a != b {
            out.push(format!("{prefix}.models: updated ({} -> {} entries)", a.len(), b.len()));
        }
        let (a, b) = (excluded(&o["excluded-models"]), excluded(&n["excluded-models"]));
        if a != b {
            out.push(format!(
                "{prefix}.excluded-models: updated ({} -> {} entries)",
                a.len(),
                b.len()
            ));
        }
        if family == "vertex" && o["headers"] != n["headers"] {
            out.push(format!("{prefix}.headers: updated"));
        }
        if family == "claude" {
            changed(
                out,
                &format!("{prefix}.rebuild-mid-system-message"),
                &o["rebuild-mid-system-message"],
                &n["rebuild-mid-system-message"],
                'b',
            );
            changed(
                out,
                &format!("{prefix}.fingerprint-profile"),
                &o["fingerprint-profile"],
                &n["fingerprint-profile"],
                't',
            );
        }
        if !matches!(family, "xai" | "meta") {
            changed(
                out,
                &format!("{prefix}.request-retry"),
                &o["request-retry"],
                &n["request-retry"],
                'p',
            );
        }
        if family == "claude" && o["cloak"].is_object() && n["cloak"].is_object() {
            changed(
                out,
                &format!("{prefix}.cloak.mode"),
                &o["cloak"]["mode"],
                &n["cloak"]["mode"],
                't',
            );
            changed(
                out,
                &format!("{prefix}.cloak.strict-mode"),
                &o["cloak"]["strict-mode"],
                &n["cloak"]["strict-mode"],
                'b',
            );
            if list(&o["cloak"]["sensitive-words"]).len() != list(&n["cloak"]["sensitive-words"]).len() {
                out.push(format!(
                    "{prefix}.cloak.sensitive-words: {} -> {}",
                    list(&o["cloak"]["sensitive-words"]).len(),
                    list(&n["cloak"]["sensitive-words"]).len()
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn real_go_reload_summaries_and_redaction() {
        let dir = std::env::temp_dir().join(format!("cpa-config-diff-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let private_paths = |value: &str| {
            value
                .replace("/tmp/cpa-diff-fixture-auth", &dir.join("auth").to_string_lossy())
                .replace("/tmp/new-auth", &dir.join("next-auth").to_string_lossy())
        };
        let cases: Value = serde_json::from_str(include_str!("../tests/fixtures/config_diff_go.json")).unwrap();
        for (i, case) in list(&cases).iter().enumerate() {
            let old = Config::parse(&private_paths(text(&case["old"]))).unwrap();
            let new = Config::parse(&private_paths(text(&case["new"]))).unwrap();
            let actual = details(&old, &new);
            let expected: Vec<_> = list(&case["changes"])
                .iter()
                .map(|value| private_paths(text(value)))
                .collect();
            assert_eq!(json!(actual), json!(expected), "case {i}");
            assert!(!actual.join("\n").contains("fixture-secret"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
