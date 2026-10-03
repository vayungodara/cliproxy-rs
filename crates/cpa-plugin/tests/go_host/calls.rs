//! The `call` steps of the Go golden: each calls the Rust counterpart of a Go `Host`
//! method (see `reference/pluginhost/capabilities.go`) and projects the result the way
//! the generator does.

use bytes::Bytes;
use cpa_plugin::Host;
use cpa_plugin::api::{FrontendAuthRequest, ModelInfo, ThinkingConfig};
use cpa_plugin::auth::PluginAuth;
use cpa_plugin::callbacks::RequestScope;
use cpa_plugin::cli::Output;
use cpa_plugin::gojson::{self, GoJson, Header, NonNilBytes};
use cpa_plugin::rpc::CallError;
use cpa_plugin::transform::ResponseTransform;
use serde_json::{Value, json};

/// A pluginapi value as Go `encoding/json` writes it.
fn gv<T: GoJson>(v: &T) -> Value {
    serde_json::from_slice(&gojson::to_vec(v)).unwrap()
}

fn decode<T: GoJson + Default>(v: &Value) -> T {
    if v.is_null() {
        return T::default();
    }
    gojson::from_slice(&serde_json::to_vec(v).unwrap()).unwrap()
}

fn s<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or_default()
}

/// Go `outcome`: a Rust `Err` is Go's `(zero, true, err)`.
fn outcome<T: Default>(result: Result<Option<T>, CallError>, project: impl Fn(&T) -> Value) -> Value {
    match result {
        Ok(Some(resp)) => json!({"resp": project(&resp), "handled": true, "error": ""}),
        Ok(None) => json!({"resp": null, "handled": false, "error": ""}),
        Err(e) => json!({"resp": project(&T::default()), "handled": true, "error": e.to_string()}),
    }
}

/// A dynamic value as Go `encoding/json` writes it (numbers decoded as float64).
fn go_any(v: &Value) -> Value {
    let mut out = Vec::new();
    gojson::encode_any(v, &mut out);
    serde_json::from_slice(&out).unwrap()
}

/// The generator's `authJSON`.
fn auth_json(auth: &PluginAuth) -> Value {
    json!({
        "id": auth.id,
        "provider": auth.provider,
        "file_name": auth.file_name,
        "label": auth.label,
        "prefix": auth.prefix,
        "proxy_url": auth.proxy_url,
        "disabled": auth.disabled,
        "status": if auth.disabled { "disabled" } else { "active" },
        "metadata": go_any(&Value::Object(auth.metadata.clone().into_iter().collect())),
        "attributes": auth.attributes,
        "next_refresh_after": gv(&auth.next_refresh_after),
        "storage": auth.storage_payload().map(|b| String::from_utf8(b.to_vec()).unwrap()).unwrap_or_default(),
    })
}

fn model_ids(models: &[ModelInfo]) -> Vec<String> {
    models.iter().map(|m| m.id.clone()).collect()
}

fn plugin_auth(host: &Host, args: &Value, file_name: &str) -> PluginAuth {
    PluginAuth::from_auth_data(
        decode(&args["req"]),
        s(args, "path"),
        file_name,
        &host.host_config_summary().auth_dir,
    )
    .unwrap()
}

pub async fn call(host: &Host, args: &Value) -> Value {
    let scope = RequestScope::default();
    let req = &args["req"];
    let body = || Bytes::from(s(args, "body").to_owned());
    let transform = || ResponseTransform {
        from: s(args, "from"),
        to: s(args, "to"),
        model: s(args, "model"),
        original_request: s(args, "original").as_bytes(),
        translated_request: s(args, "request").as_bytes(),
        stream: flag(args, "stream"),
    };
    let text = |b: Bytes| Value::from(String::from_utf8(b.to_vec()).unwrap());
    match s(args, "fn") {
        "has" => json!({
            "request_interceptors": host.has_request_interceptors(),
            "stream_interceptors": host.has_stream_interceptors(),
            "stream_request_body": host.stream_chunk_payload_includes_request_body(),
            "stream_history": host.stream_chunk_payload_includes_history(),
            "websocket_observers": host.has_websocket_response_observers(),
            "scheduler": host.has_scheduler(),
            "scheduler_across": host.scheduler_wants_across_priorities(),
            "model_routers": host.has_model_routers(""),
            "quota_identifiers": host.quota_provider_identifiers(),
            "auth_identifiers": host.auth_provider_identifiers(),
            "auth_provider_rec_c": host.has_auth_provider("REC-C"),
            "quota_provider_plugin_a": host.has_quota_provider_for_plugin("recorder-a"),
        }),
        "intercept_before" => gv(&host.intercept_request_before_auth(decode(req), "", &scope).await),
        "intercept_after" => gv(&host.intercept_request_after_auth(decode(req), "", &scope).await),
        "intercept_response" => gv(&host.intercept_response(decode(req), "", &scope).await),
        "intercept_stream_chunk" => gv(&host.intercept_stream_chunk(decode(req), "", &scope).await),
        "complete" => {
            host.complete_request(decode(req), "", &scope);
            Value::Null
        }
        "ws_event" => {
            host.observe_websocket_response_event(decode(req), "", &scope).await;
            Value::Null
        }
        "pick_auth" => outcome(host.pick_auth(decode(req)).await, gv),
        "route_model" => outcome(Ok(host.route_model(decode(req), "", &[], &scope).await), gv),
        "normalize_request" => text(
            host.normalize_request(
                s(args, "from"),
                s(args, "to"),
                s(args, "model"),
                body(),
                flag(args, "stream"),
            )
            .await,
        ),
        "translate_request" => {
            let input = body();
            match host
                .translate_request(
                    s(args, "from"),
                    s(args, "to"),
                    s(args, "model"),
                    &input,
                    flag(args, "stream"),
                )
                .await
            {
                Some(out) => json!({"body": text(out), "ok": true}),
                None => json!({"body": text(input), "ok": false}),
            }
        }
        "normalize_response_before" => text(host.normalize_response_before(&transform(), body()).await),
        "translate_response" => {
            let input = body();
            match host.translate_response(&transform(), &input).await {
                Some(out) => json!({"body": text(out), "ok": true}),
                None => json!({"body": text(input), "ok": false}),
            }
        }
        "normalize_response_after" => text(host.normalize_response_after(&transform(), body()).await),
        "thinking" => {
            let provider = s(args, "provider");
            let plugin = host.thinking_provider(provider).is_some();
            let out = if plugin {
                let model = ModelInfo {
                    id: s(args, "model").into(),
                    object: "model".into(),
                    owned_by: "tests".into(),
                    model_type: "recorder".into(),
                    ..Default::default()
                };
                let config = ThinkingConfig {
                    mode: "budget".into(),
                    budget: args.get("budget").and_then(Value::as_i64).unwrap_or_default(),
                    level: s(args, "level").into(),
                };
                host.apply_thinking(provider, model, config, body(), &scope).await
            } else {
                body()
            };
            json!({"plugin": plugin, "body": text(out)})
        }
        "auth_data" => {
            match PluginAuth::from_auth_data(
                decode(req),
                s(args, "path"),
                s(args, "file_name"),
                &host.host_config_summary().auth_dir,
            ) {
                Some(auth) => auth_json(&auth),
                None => Value::Null,
            }
        }
        "parse_auths" => {
            let res = host.parse_auths(decode(req)).await;
            let resp = res
                .handled
                .then(|| Value::Array(res.auths.iter().map(auth_json).collect()));
            json!({"resp": resp, "handled": res.handled, "error": res.error.map(|e| e.to_string()).unwrap_or_default()})
        }
        "start_login" => {
            let metadata = decode(args.get("metadata").unwrap_or(&Value::Null));
            outcome(
                host.start_login(s(args, "provider"), s(args, "base_url"), metadata, &scope)
                    .await,
                gv,
            )
        }
        "poll_login" => {
            let metadata = decode(args.get("metadata").unwrap_or(&Value::Null));
            outcome(
                host.poll_login(s(args, "provider"), s(args, "state"), metadata, &scope)
                    .await,
                gv,
            )
        }
        "refresh_auth" => {
            let auth = plugin_auth(host, args, "").view();
            match host.refresh_auth(&auth, &scope).await {
                Ok(Some(refreshed)) => json!({"resp": auth_json(&refreshed), "handled": true, "error": ""}),
                Ok(None) => json!({"resp": null, "handled": false, "error": ""}),
                Err(e) => json!({"resp": null, "handled": true, "error": e.to_string()}),
            }
        }
        "quota_providers" => Value::Array(
            host.quota_providers()
                .await
                .into_iter()
                .map(|p| {
                    let mut v =
                        json!({"plugin_id": p.plugin_id, "provider": p.provider, "supports_reset": p.supports_reset});
                    if !p.display_name.is_empty() {
                        v["display_name"] = p.display_name.into();
                    }
                    if !p.supported_providers.is_empty() {
                        v["supported_providers"] = p.supported_providers.into();
                    }
                    v
                })
                .collect(),
        ),
        "describe_quota" => outcome(host.describe_quota(s(args, "plugin_id")).await, gv),
        "fetch_quota" => outcome(host.fetch_quota(decode(req), None, &scope).await, gv),
        "fetch_quota_by_plugin" => outcome(
            host.fetch_quota_by_plugin(s(args, "plugin_id"), decode(req), None, &scope)
                .await,
            gv,
        ),
        "reset_quota" => outcome(host.reset_quota(decode(req), None, &scope).await, gv),
        "reset_quota_by_plugin" => outcome(
            host.reset_quota_by_plugin(s(args, "plugin_id"), decode(req), None, &scope)
                .await,
            gv,
        ),
        "register_models" => {
            let changes = host.register_models().await;
            let mut calls: Vec<Value> = changes
                .register
                .iter()
                .map(|(client, provider, models)| json!({"op": "register", "client": client, "provider": provider, "models": model_ids(models)}))
                .collect();
            calls.extend(
                changes
                    .unregister
                    .iter()
                    .map(|client| json!({"op": "unregister", "client": client})),
            );
            Value::Array(calls)
        }
        "models_for_auth" => {
            let auth = plugin_auth(host, args, "").view();
            let res = host.models_for_auth(&auth, &scope).await;
            json!({
                "provider": res.provider,
                "models": model_ids(&res.models),
                "auth": res.auth.as_ref().map(auth_json),
                "handled": res.handled,
                "error": res.error.map(|e| e.to_string()).unwrap_or_default(),
            })
        }
        "frontend_auth" => {
            let target = s(args, "target");
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let mut q = Header::new();
            for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
                q.entry(k.into_owned()).or_default().push(v.into_owned());
            }
            let mut headers = Header::new();
            if let Some(h) = args.get("headers").and_then(Value::as_object) {
                for (k, vs) in h {
                    for v in vs.as_array().unwrap() {
                        headers
                            .entry(cpa_exec::proxy::canonical_header(k))
                            .or_default()
                            .push(v.as_str().unwrap().into());
                    }
                }
            }
            let request = FrontendAuthRequest {
                method: s(args, "method").into(),
                path: path.into(),
                headers,
                query: q,
                body: NonNilBytes(body()),
            };
            let (providers, exclusive) = host.frontend_auth_providers().await;
            let providers: Vec<_> = match exclusive {
                Some(key) => providers.into_iter().filter(|(k, _)| *k == key).collect(),
                None => providers,
            };
            let mut out = Vec::new();
            for (_, plugin_id) in providers {
                let res = host.frontend_authenticate(&plugin_id, &request).await;
                let key = host.frontend_auth_identifier(&plugin_id).await.unwrap_or_default();
                out.push(match res {
                    Some(res) => json!({"provider": key, "error": "", "result": {"provider": res.provider, "principal": res.principal, "metadata": res.metadata}}),
                    None => json!({"provider": key, "error": "not_handled", "result": null}),
                });
            }
            Value::Array(out)
        }
        "command_line" => command_line(host, args).await,
        other => panic!("unknown call {other}"),
    }
}

/// Go's `flag.FlagSet` with the builtin string flags plus the plugins' flags, parsed by
/// Go's rules, then `ExecuteCommandLine`.
async fn command_line(host: &Host, args: &Value) -> Value {
    let builtin: Vec<String> = args["builtin"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let argv: Vec<String> = args["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let accepted = host
        .register_command_line_flags(&|name| builtin.iter().any(|b| b == name))
        .await;
    let mut names: Vec<String> = builtin.iter().map(|b| format!("{b}=")).collect();
    names.extend(accepted.iter().map(|f| format!("{}={}", f.name, f.value)));
    names.sort();
    // Go flag.Parse: stops at the first non-flag or "--".
    let mut values: Vec<(String, String)> = builtin.iter().map(|b| (b.clone(), String::new())).collect();
    let mut parse_error = String::new();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if arg.len() < 2 || !arg.starts_with('-') || arg == "--" {
            break;
        }
        let name = arg.trim_start_matches('-');
        let (name, inline) = match name.split_once('=') {
            Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
            None => (name.to_owned(), None),
        };
        i += 1;
        let is_bool = host.command_line_flag(&name).is_some_and(|f| f.kind == "bool");
        let value = match inline {
            Some(v) => v,
            None if is_bool => "true".to_owned(),
            None => {
                let v = argv[i].clone();
                i += 1;
                v
            }
        };
        if let Some(slot) = values.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = value;
        } else if host.command_line_flag(&name).is_some() {
            if let Err(e) = host.set_command_line_flag(&name, &value) {
                parse_error = e;
                break;
            }
        } else {
            parse_error = format!("flag provided but not defined: -{name}");
            break;
        }
    }
    let persist = |_auth: PluginAuth| -> Result<String, String> { Err("unexpected save".into()) };
    let (exit, handled, output) = host
        .execute_command_line("cliproxy", &argv, "/etc/cliproxy/config.yaml", &values, &persist)
        .await;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for item in output {
        match item {
            Output::Stdout(b) => stdout.extend_from_slice(&b),
            Output::Stderr(b) => stderr.extend_from_slice(&b),
        }
    }
    json!({
        "flags": names,
        "parse_error": parse_error,
        "triggered": host.has_triggered_command_line_flags(),
        "exit": exit,
        "handled": handled,
        "stdout": String::from_utf8(stdout).unwrap(),
        "stderr": String::from_utf8(stderr).unwrap(),
    })
}
