//! The provider-neutral attempt loop every inference route runs (Go
//! `BaseAPIHandler.Execute*WithAuthManager` plus `Manager.Execute*`).
//!
//! Model to providers through the registry, credential selection across all of them,
//! per-credential alias pools, request-scoped rules, retry rounds and stream bootstrap.
//! Routes only parse their request and render the [`Done`] or [`Failure`].

use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use bytes::{Bytes, BytesMut};
use cpa_core::config::Config;
use cpa_core::exec::{Caller, ExecError, ExecRequest, ExecStream, Operation, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;

use crate::classify;
use crate::gojson;
use crate::registry::{self, AliasResult, Registry};
use crate::runtime::{AcquireError, Completing, Lease, Outcome, Runtime, Selection};
use crate::scheduler::Policy;

/// One client request, already parsed by its route.
#[derive(Clone)]
pub struct Call {
    /// Format of the client request (Go entry protocol).
    pub entry: Format,
    /// Format the client expects back.
    pub response: Format,
    pub operation: Operation,
    /// The model exactly as the client named it (gjson `String()`, untrimmed).
    pub model: String,
    pub body: Bytes,
    pub stream: bool,
    pub alt: Option<String>,
    pub headers: HeaderMap,
    pub caller: Caller,
    /// Skip registry routing and use this provider (Interactions agents).
    pub forced_provider: Option<String>,
    /// Model used for credential selection when it differs from `model`.
    pub selection_model: Option<String>,
}

pub enum Done {
    Buffered {
        headers: HeaderMap,
        body: Bytes,
    },
    /// A stream whose first event already arrived. `rest` reports its lease outcome.
    Stream {
        headers: HeaderMap,
        first: Option<Bytes>,
        rest: ExecStream,
    },
}

#[derive(Debug)]
pub enum Failure {
    /// An executor or upstream error, rendered by the route's error shape.
    Exec(ExecError),
    /// No provider registered the model (Go `getRequestDetails`).
    UnknownModel(String),
    /// An image-only model on a non-image route.
    ImageOnly(String),
    /// `auth_not_found` / `auth_unavailable`, enriched like Go.
    Unavailable {
        code: &'static str,
        providers: Vec<String>,
        model: String,
        cause: Option<String>,
        retry_after: Option<Duration>,
    },
    /// Every candidate is cooling down for this model.
    Cooldown {
        model: String,
        provider: String,
        wait: Duration,
        cause: Option<String>,
    },
}

impl Failure {
    pub fn status(&self) -> u16 {
        match self {
            Failure::Exec(e) => classify::response_status(e),
            Failure::UnknownModel(_) => 400,
            Failure::ImageOnly(_) | Failure::Unavailable { .. } => 503,
            Failure::Cooldown { .. } => 429,
        }
    }

    /// Go `err.Error()`: what route error writers render.
    pub fn text(&self) -> String {
        match self {
            Failure::Exec(e) => classify::error_text(e),
            Failure::UnknownModel(model) => {
                let message = crate::jsonedit::sjson_string(&format!("unknown provider for model {model}"));
                format!(
                    r#"{{"error":{{"message":{message},"type":"invalid_request_error","code":"model_not_found","param":"model"}}}}"#
                )
            }
            Failure::ImageOnly(model) => {
                let base = model.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(model);
                format!("model {} is only supported on /v1/images/generations and /v1/images/edits", base.trim())
            }
            Failure::Unavailable {
                code,
                providers,
                model,
                cause,
                ..
            } => {
                let providers = if providers.is_empty() { "unknown".into() } else { providers.join(",") };
                let model = if gojson::trim(model).is_empty() { "unknown" } else { gojson::trim(model) };
                let mut detail = match cause.as_deref().map(upstream_summary).filter(|s| !s.is_empty()) {
                    Some(summary) => format!(
                        "no auth available (providers={providers}, model={model}; last upstream error: {summary})"
                    ),
                    None => format!("no auth available (providers={providers}, model={model})"),
                };
                if format!(",{providers},").contains(",claude,") {
                    detail.push_str("; check Claude auth/key session and cooldown state via /v0/management/auth-files");
                }
                format!("{code}: {detail}")
            }
            Failure::Cooldown {
                model,
                provider,
                wait,
                cause,
            } => {
                let shown = if model.is_empty() { "requested model" } else { model };
                let mut message = format!("All credentials for model {shown} are cooling down");
                if !provider.is_empty() {
                    message.push_str(&format!(" via provider {provider}"));
                }
                let display = if !wait.is_zero() && *wait < Duration::from_secs(1) {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs((wait.as_secs_f64()).round() as u64)
                };
                let mut body = serde_json::Map::new();
                body.insert("code".into(), "model_cooldown".into());
                body.insert("model".into(), model.clone().into());
                body.insert("reset_time".into(), gojson::duration(display).into());
                body.insert("reset_seconds".into(), ceil_seconds(*wait).into());
                if !provider.is_empty() {
                    body.insert("provider".into(), provider.clone().into());
                }
                if let Some(summary) = cause.as_deref().map(upstream_summary).filter(|s| !s.is_empty()) {
                    message.push_str(&format!(" (last error: {summary})"));
                    body.insert("last_upstream_error".into(), summary.into());
                }
                body.insert("message".into(), message.into());
                gojson::sorted(&serde_json::json!({ "error": body }))
            }
        }
    }

    /// `Retry-After` from Go's `SafeResponseHeaders` (scheduler errors only).
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Failure::Cooldown { wait, .. } => Some(ceil_seconds(*wait)),
            Failure::Unavailable {
                retry_after: Some(wait),
                ..
            } if !wait.is_zero() => Some(ceil_seconds(*wait).max(1)),
            _ => None,
        }
    }

    /// An error the route returns untouched (claude_executor_fast_error.go).
    pub fn direct(&self) -> Option<&ExecError> {
        match self {
            Failure::Exec(e) if e.direct => Some(e),
            _ => None,
        }
    }
}

fn ceil_seconds(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

/// Go `ExtractUpstreamErrorSummary`, minus path redaction.
// ponytail: Go also redacts URLs, query secrets and filesystem paths in summaries;
// port SanitizeUpstreamErrorSummary if upstream errors start echoing such values.
pub fn upstream_summary(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let json_part = match raw.find(": {") {
        Some(i) if i < 50 => raw[i + 2..].trim(),
        _ => raw,
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_part) {
        let s = |v: Option<&serde_json::Value>| gojson::trim(&gojson::gjson_string(v)).to_owned();
        let (mut code, mut message) = (String::new(), String::new());
        match v.get("error") {
            Some(e @ serde_json::Value::Object(_)) => {
                code = s(e.get("code"));
                if code.is_empty() {
                    code = s(e.get("type"));
                }
                message = s(e.get("message"));
            }
            Some(serde_json::Value::String(m)) => message = m.trim().to_owned(),
            _ => {}
        }
        if code.is_empty() && message.is_empty() {
            code = s(v.get("code"));
            if code.is_empty() {
                code = s(v.get("type"));
            }
            message = s(v.get("message"));
        }
        let summary = match (code.is_empty(), message.is_empty()) {
            (false, false) if code.eq_ignore_ascii_case(&message) || message.to_lowercase().contains(&code.to_lowercase()) => message,
            (false, false) => format!("{code}: {message}"),
            (true, false) => message,
            (false, true) => code,
            (true, true) => String::new(),
        };
        if !summary.is_empty() {
            return truncate(&summary);
        }
    }
    truncate(raw)
}

fn truncate(s: &str) -> String {
    const LIMIT: usize = 512;
    if s.chars().count() <= LIMIT {
        s.to_owned()
    } else {
        s.chars().take(LIMIT).collect::<String>() + "..."
    }
}

const IMAGE_ONLY: [&str; 8] = [
    "gpt-image-1.5",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
    "grok-imagine-image",
    "grok-imagine-image-quality",
    "grok-imagine-image-2.0",
];

/// Go `getRequestDetails`: resolve `auto`, find the providers, keep the suffix.
fn route(registry: &Registry, call: &Call) -> Result<(Vec<String>, String), Failure> {
    if let Some(provider) = &call.forced_provider {
        return Ok((vec![provider.clone()], gojson::trim(&call.model).to_owned()));
    }
    let model = call.model.as_str();
    let base = match model.rfind('(') {
        Some(i) if model.ends_with(')') => &model[..i],
        _ => model,
    };
    let resolved = if base == "auto" {
        let first = registry.resolve_auto().unwrap_or_else(|| "auto".into());
        format!("{first}{}", &model[base.len()..])
    } else {
        model.to_owned()
    };
    let base = gojson::trim(canonical_model_raw(&resolved)).to_owned();
    let image = base.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(&base).trim().to_lowercase();
    if IMAGE_ONLY.contains(&image.as_str()) {
        return Err(Failure::ImageOnly(base));
    }
    let mut providers = registry.providers(&base);
    if providers.is_empty() && base != resolved {
        providers = registry.providers(&resolved);
    }
    if providers.is_empty() {
        return Err(Failure::UnknownModel(call.model.clone()));
    }
    // Go `adjustExecutionProvidersForEntryProtocol`.
    match call.entry {
        Format::Interactions => {
            if let Some(i) = providers.iter().position(|p| p == "gemini-interactions") {
                let p = providers.remove(i);
                providers.insert(0, p);
            }
        }
        Format::OpenAI | Format::OpenAIResponse | Format::Claude | Format::Gemini => {}
        _ => providers.retain(|p| p != "gemini-interactions"),
    }
    Ok((providers, resolved))
}

/// `thinking.ParseSuffix(model).ModelName` without trimming.
fn canonical_model_raw(model: &str) -> &str {
    match model.rfind('(') {
        Some(i) if model.ends_with(')') => &model[..i],
        _ => model,
    }
}

/// Runs one request through selection, execution and retry rounds.
pub async fn run(rt: &Arc<Runtime>, call: Call) -> Result<Done, Failure> {
    let (cfg, policy) = rt.request_snapshot();
    let registry = rt.registry();
    let (providers, model) = route(&registry, &call)?;
    let aliases = registry::global_aliases(&cfg);
    let session = crate::session::resolve(&call.headers, &call.body);
    let mut selection = Selection {
        providers: providers.clone(),
        model: call.selection_model.clone().unwrap_or_else(|| model.clone()),
        session: session.clone(),
        forced: call.forced_provider.is_some(),
        ..Selection::default()
    };
    let request = ExecRequest {
        operation: call.operation,
        source_format: call.entry,
        response_format: call.response,
        requested_model: model.clone(),
        model: model.clone(),
        original_body: call.body.clone(),
        body: call.body.clone(),
        stream: call.stream,
        alt: call.alt.clone(),
        session,
        headers: call.headers.clone(),
        caller: call.caller.clone(),
    };
    let compact = call.alt.as_deref() == Some("responses/compact");
    loop {
        let mut last_error: Option<ExecError> = None;
        let mut attempted = 0;
        let outcome = loop {
            if policy.max_retry_credentials > 0 && attempted >= policy.max_retry_credentials {
                break None;
            }
            let lease = match rt.acquire(selection.clone(), &cfg, policy.clone(), &registry).await {
                Ok(lease) => lease,
                Err(AcquireError::Prepare { id, error }) => {
                    attempted += 1;
                    selection.exclude.push(id);
                    last_error = Some(error);
                    continue;
                }
                Err(AcquireError::Cooldown { wait, cause }) => {
                    if last_error.is_some() {
                        break None;
                    }
                    let provider = if providers.len() == 1 { providers[0].clone() } else { String::new() };
                    return Err(Failure::Cooldown {
                        model: selection.model.clone(),
                        provider,
                        wait,
                        cause,
                    });
                }
                Err(AcquireError::Unavailable { retry_after, cause }) => {
                    if last_error.is_some() {
                        break None;
                    }
                    return Err(Failure::Unavailable {
                        code: if retry_after.is_some() { "auth_unavailable" } else { "auth_not_found" },
                        providers: providers.clone(),
                        model: model.clone(),
                        cause,
                        retry_after,
                    });
                }
            };
            selection.exclude.push(lease.credential.id.clone());
            let (models, alias) = registry::execution_models(&aliases, &lease.credential, &selection.model);
            let pooled = models.len() > 1;
            let selection_model = registry::selection_model(&aliases, &lease.credential, &selection.model);
            let models: Vec<String> = models
                .into_iter()
                .filter(|m| {
                    let state = registry::state_model(&selection_model, &selection.model, m, pooled);
                    !rt.store().blocked(&lease.credential, &state)
                })
                .collect();
            if models.is_empty() {
                continue;
            }
            attempted += 1;
            match attempt(rt, &cfg, &policy, &call, &request, lease, &models, &selection_model, pooled, &alias, compact).await {
                Attempt::Done(done) => break Some(Ok(done)),
                Attempt::Stop(error) => break Some(Err(error)),
                Attempt::Next(error) => last_error = Some(error),
            }
        };
        match outcome {
            Some(Ok(done)) => return Ok(done),
            Some(Err(error)) => return Err(Failure::Exec(error)),
            None => {}
        }
        let Some(error) = last_error else {
            return Err(Failure::Unavailable {
                code: "auth_not_found",
                providers,
                model,
                cause: None,
                retry_after: None,
            });
        };
        if !classify::is_retry_round(&error) || classify::is_request_invalid(&error) {
            return Err(Failure::Exec(error));
        }
        let wait = {
            let admit = crate::runtime::admission(&registry, &aliases, &selection, &rt.executors);
            rt.store().retry_wait(&selection, &policy, &error, &admit)
        };
        let Some(wait) = wait else {
            return Err(Failure::Exec(error));
        };
        if !wait.is_zero() {
            tokio::time::sleep(jitter(wait, policy.max_retry_interval)).await;
        }
        selection.retry_round += 1;
        selection.exclude.clear();
    }
}

/// Go `jitteredCooldownWait`: up to a quarter of the wait (at most 2s) extra, never past
/// the configured maximum, so synchronized clients spread out.
pub fn jitter(wait: Duration, max: Duration) -> Duration {
    let mut range = (wait / 4).min(Duration::from_secs(2));
    if !max.is_zero() {
        range = range.min(max.saturating_sub(wait));
    }
    if range.is_zero() {
        return wait;
    }
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    wait + Duration::from_nanos(hasher.finish() % range.as_nanos().max(1) as u64)
}

enum Attempt {
    Done(Done),
    /// Terminal for the whole request.
    Stop(ExecError),
    /// Failed over; try the next credential.
    Next(ExecError),
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    rt: &Arc<Runtime>,
    cfg: &Config,
    policy: &Policy,
    call: &Call,
    request: &ExecRequest,
    mut lease: Lease,
    models: &[String],
    selection_model: &str,
    pooled: bool,
    alias: &AliasResult,
    compact: bool,
) -> Attempt {
    let route_model = lease.selection.model.clone();
    let mut last = None;
    for (i, upstream) in models.iter().enumerate() {
        let state = registry::state_model(selection_model, &route_model, upstream, pooled);
        lease.execution_model = state.clone();
        let mut req = request.clone();
        req.model.clone_from(upstream);
        let error = match rt.executors.execute(&lease.credential, req, cfg).await {
            Ok(response) => match finish(call, response).await {
                Ok(done) => {
                    let done = if alias.force_mapping && !alias.original_alias.is_empty() {
                        rewrite_model(done, &alias.original_alias)
                    } else {
                        done
                    };
                    return Attempt::Done(match done {
                        Done::Stream { headers, first, rest } => Done::Stream {
                            headers,
                            first,
                            rest: Completing::new(rest, lease).boxed(),
                        },
                        buffered => {
                            lease.complete(Outcome::Success);
                            buffered
                        }
                    });
                }
                Err(error) => error,
            },
            Err(error) => error,
        };
        let action = policy.error_action(&lease.credential, &error);
        let neutral = (compact && classify::is_compact_neutral(&error) && !action.force_cooldown)
            || (call.operation == Operation::CountTokens
                && classify::is_count_endpoint_missing(&error, upstream)
                && !action.force_cooldown);
        let outcome = if neutral {
            Outcome::Neutral(error.clone())
        } else {
            Outcome::Failure(error.clone())
        };
        let stop = if action.matched {
            action.stop
        } else {
            (compact && classify::is_compact_fault(&error)) || classify::is_request_invalid(&error)
        };
        // A credential-wide quota ends this credential's model pool.
        if stop || i + 1 == models.len() || classify::credential_scoped(&error) {
            lease.complete(outcome);
            return if stop { Attempt::Stop(error) } else { Attempt::Next(error) };
        }
        lease.note(&state, &outcome);
        last = Some(error);
    }
    Attempt::Next(last.expect("at least one model was attempted"))
}

/// Bootstraps a stream (first event before committing) or buffers a body.
async fn finish(call: &Call, response: cpa_core::exec::ExecResponse) -> Result<Done, ExecError> {
    match response.body {
        ResponseBody::Buffered(body) => Ok(Done::Buffered {
            headers: response.headers,
            body,
        }),
        ResponseBody::Stream(mut stream) if call.stream => {
            let first = loop {
                match stream.next().await {
                    Some(Ok(bytes)) if bytes.is_empty() => continue,
                    Some(Ok(bytes)) => break Some(bytes),
                    Some(Err(error)) => return Err(error),
                    None => break None,
                }
            };
            Ok(Done::Stream {
                headers: response.headers,
                first,
                rest: stream,
            })
        }
        ResponseBody::Stream(mut stream) => {
            let mut body = BytesMut::new();
            while let Some(event) = stream.next().await {
                body.extend_from_slice(&event?);
            }
            Ok(Done::Buffered {
                headers: response.headers,
                body: body.freeze(),
            })
        }
    }
}

/// Go `rewriteModelInResponse` over a buffered body or each SSE data line.
fn rewrite_model(done: Done, target: &str) -> Done {
    match done {
        Done::Buffered { headers, body } => Done::Buffered {
            headers,
            body: rewrite_payload(&body, target),
        },
        Done::Stream { headers, first, rest } => {
            let target_owned = target.to_owned();
            let rest = rest
                .map(move |item| item.map(|event| rewrite_payload(&event, &target_owned)))
                .boxed();
            Done::Stream {
                headers,
                first: first.map(|event| rewrite_payload(&event, target)),
                rest,
            }
        }
    }
}

const MODEL_PATHS: [&str; 5] = ["model", "modelVersion", "response.model", "response.modelVersion", "message.model"];

fn rewrite_json(data: &str, target: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(data).ok()?;
    let mut changed = false;
    for path in MODEL_PATHS {
        let mut node = Some(&mut value);
        let parts: Vec<&str> = path.split('.').collect();
        for part in &parts[..parts.len() - 1] {
            node = node.and_then(|n| n.get_mut(*part));
        }
        if let Some(slot) = node.and_then(|n| n.get_mut(parts[parts.len() - 1])) {
            *slot = target.into();
            changed = true;
        }
    }
    changed.then(|| value.to_string())
}

fn rewrite_payload(payload: &Bytes, target: &str) -> Bytes {
    let Ok(text) = std::str::from_utf8(payload) else {
        return payload.clone();
    };
    if text.trim_start().starts_with('{') {
        return rewrite_json(text, target).map(Bytes::from).unwrap_or_else(|| payload.clone());
    }
    let mut changed = false;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            let Some(rest) = line.strip_prefix("data:") else {
                return line.to_owned();
            };
            let data = rest.trim_start();
            let prefix = &line[..line.len() - data.len()];
            match data.starts_with('{').then(|| rewrite_json(data.trim_end_matches('\r'), target)).flatten() {
                Some(json) => {
                    changed = true;
                    format!("{prefix}{json}{}", if data.ends_with('\r') { "\r" } else { "" })
                }
                None => line.to_owned(),
            }
        })
        .collect();
    if changed { Bytes::from(lines.join("\n")) } else { payload.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_texts_match_go_shapes() {
        assert_eq!(
            Failure::UnknownModel("claude-sonnet-4-6".into()).text(),
            r#"{"error":{"message":"unknown provider for model claude-sonnet-4-6","type":"invalid_request_error","code":"model_not_found","param":"model"}}"#
        );
        let cooldown = Failure::Cooldown {
            model: "m".into(),
            provider: "claude".into(),
            wait: Duration::from_millis(59_400),
            cause: Some(r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#.into()),
        };
        assert_eq!(
            cooldown.text(),
            r#"{"error":{"code":"model_cooldown","last_upstream_error":"rate_limit_error: slow down","message":"All credentials for model m are cooling down via provider claude (last error: rate_limit_error: slow down)","model":"m","provider":"claude","reset_seconds":60,"reset_time":"59s"}}"#
        );
        assert_eq!(cooldown.retry_after(), Some(60));
        assert_eq!((cooldown.status(), Failure::UnknownModel(String::new()).status()), (429, 400));
        let none = Failure::Unavailable {
            code: "auth_not_found",
            providers: vec!["claude".into()],
            model: " m ".into(),
            cause: None,
            retry_after: None,
        };
        assert_eq!(
            none.text(),
            "auth_not_found: no auth available (providers=claude, model=m); check Claude auth/key session and cooldown state via /v0/management/auth-files"
        );
        assert_eq!((none.status(), none.retry_after()), (503, None));
    }

    #[test]
    fn force_mapping_rewrites_json_and_sse_model_fields() {
        let body = Bytes::from_static(br#"{"id":"x","model":"claude-opus-5","message":{"model":"claude-opus-5"}}"#);
        assert_eq!(
            rewrite_payload(&body, "opus"),
            r#"{"id":"x","model":"opus","message":{"model":"opus"}}"#
        );
        let event = Bytes::from_static(b"event: message_start\ndata: {\"message\":{\"model\":\"up\"}}\n\n");
        assert_eq!(
            rewrite_payload(&event, "alias"),
            "event: message_start\ndata: {\"message\":{\"model\":\"alias\"}}\n\n"
        );
        let untouched = Bytes::from_static(b"data: [DONE]\n\n");
        assert_eq!(rewrite_payload(&untouched, "alias"), untouched);
    }

    #[test]
    fn jitter_stays_within_go_bounds() {
        for _ in 0..50 {
            let w = jitter(Duration::from_secs(4), Duration::ZERO);
            assert!(w >= Duration::from_secs(4) && w < Duration::from_secs(5));
            let capped = jitter(Duration::from_secs(20), Duration::from_secs(21));
            assert!(capped >= Duration::from_secs(20) && capped < Duration::from_secs(21));
        }
        assert_eq!(jitter(Duration::from_secs(5), Duration::from_secs(5)), Duration::from_secs(5));
        assert_eq!(jitter(Duration::ZERO, Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn summaries_follow_go_extract() {
        assert_eq!(upstream_summary(r#"{"error":{"code":"x","message":"boom"}}"#), "x: boom");
        assert_eq!(upstream_summary(r#"{"error":{"type":"overloaded","message":"overloaded now"}}"#), "overloaded now");
        assert_eq!(upstream_summary(r#"{"error":"flat"}"#), "flat");
        assert_eq!(upstream_summary("plain text"), "plain text");
        assert_eq!(upstream_summary("status 500: {\"message\":\"m\"}"), "m");
    }
}
