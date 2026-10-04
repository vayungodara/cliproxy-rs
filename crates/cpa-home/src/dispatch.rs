//! Home's dispatch replies (Go sdk/cliproxy/auth/home_concurrency.go and the decoding
//! half of `pickHomeDispatchSelection`): the accounted concurrency tuple, Home's typed
//! errors and the dispatched auth.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};

const MAX_TUPLE_FIELD: usize = 256;
const ASCII_WHITESPACE: &[char] = &[' ', '\t', '\r', '\n', '\u{b}', '\u{c}'];

/// Go `recognizedHomeConcurrencySuffix`: thinking suffixes that do not change the
/// limiter key.
fn recognized_suffix(value: &str) -> bool {
    if value == "-1" {
        return true;
    }
    if matches!(
        value.to_lowercase().as_str(),
        "none" | "auto" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        return true;
    }
    !value.is_empty()
        && value.len() <= 10
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|n| n <= 2_147_483_647)
}

/// Go `canonicalHomeConcurrencyModelKey`: lowercase, trimmed, recognized `(suffix)`
/// removed.
pub fn canonical_concurrency_model_key(model: &str) -> String {
    let trimmed = model.trim_matches(ASCII_WHITESPACE).to_lowercase();
    let Some(body) = trimmed.strip_suffix(')') else {
        return trimmed;
    };
    let Some(open) = body.rfind('(') else {
        return trimmed;
    };
    if !recognized_suffix(&body[open + 1..]) {
        return trimmed;
    }
    let base = body[..open].trim_matches(ASCII_WHITESPACE);
    if base.is_empty() {
        return trimmed;
    }
    base.to_owned()
}

/// Go `validCanonicalHomeConcurrencyModelKey`.
pub fn valid_concurrency_model_key(model: &str) -> Option<String> {
    let key = canonical_concurrency_model_key(model);
    (!key.is_empty() && key.len() <= MAX_TUPLE_FIELD).then_some(key)
}

/// Go `homeConcurrencyTuple`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ConcurrencyTuple {
    pub accounted: bool,
    pub credential_id: String,
    pub model: String,
}

impl ConcurrencyTuple {
    /// Go `validateAccountedHomeConcurrencyTuple`.
    pub fn validate(&self) -> Result<(), String> {
        let field_ok = |v: &str| !v.is_empty() && v.trim_matches(ASCII_WHITESPACE) == v && v.len() <= MAX_TUPLE_FIELD;
        let model_ok = valid_concurrency_model_key(&self.model).is_some_and(|key| key == self.model);
        if !self.accounted || !field_ok(&self.credential_id) || !model_ok {
            return Err("malformed Home concurrency tuple".into());
        }
        Ok(())
    }
}

/// Go `decodeHomeDispatchConcurrencyEnvelope`. `Err((present, message))`.
pub fn decode_concurrency(raw: &[u8]) -> Result<Option<ConcurrencyTuple>, (bool, String)> {
    if std::str::from_utf8(raw).is_err() {
        return Err((false, "Home response is not valid UTF-8".into()));
    }
    let Ok(fields) = serde_json::from_slice::<Map<String, Value>>(raw) else {
        return Err((false, "Home response is not a JSON object".into()));
    };
    let Some(tuple) = fields.get("concurrency") else {
        return Ok(None);
    };
    let tuple: ConcurrencyTuple = serde_json::from_value(tuple.clone()).map_err(|e| (true, e.to_string()))?;
    tuple.validate().map_err(|e| (true, e))?;
    Ok(Some(tuple))
}

/// How a Home error affects retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeErrorKind {
    Plain,
    /// `model_cooldown`: wait, possibly bounded by Home's request retry limit.
    Cooldown {
        retry_after: Option<Duration>,
        request_retry: Option<i64>,
    },
    /// Credential concurrency limit: busy, retry after a jittered delay.
    Busy {
        retry_after: Option<Duration>,
    },
}

/// A typed error reply from Home (Go `decodeHomeDispatchError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub status: u16,
    pub kind: HomeErrorKind,
}

impl HomeError {
    fn plain(code: &str, message: &str, status: u16) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            status,
            kind: HomeErrorKind::Plain,
        }
    }

    /// Go `SafeResponseHeaders`: the `Retry-After` seconds this error may expose.
    pub fn retry_after_header(&self) -> Option<u64> {
        match self.kind {
            HomeErrorKind::Busy { retry_after } | HomeErrorKind::Cooldown { retry_after, .. } => {
                retry_after.and_then(retry_after_seconds)
            }
            HomeErrorKind::Plain => None,
        }
    }
}

/// Go `safeRetryAfterHeader`: whole seconds, rounded up, at least one.
pub fn retry_after_seconds(retry_after: Duration) -> Option<u64> {
    if retry_after.is_zero() {
        return None;
    }
    Some(retry_after.as_nanos().div_ceil(1_000_000_000).max(1) as u64)
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    retryable: bool,
    #[serde(default)]
    retry_after_ms: i64,
    #[serde(default)]
    request_retry: Option<i64>,
}

fn positive_millis(ms: i64) -> Option<Duration> {
    (ms > 0).then(|| Duration::from_millis(ms as u64))
}

/// Go `decodeHomeDispatchError`: `None` unless the reply carries an `error` object.
pub fn decode_error(raw: &[u8]) -> Option<HomeError> {
    let fields = serde_json::from_slice::<Map<String, Value>>(raw).ok()?;
    let detail = fields.get("error")?;
    let malformed = || HomeError::plain("invalid_auth", "home returned malformed error payload", 502);
    let Ok(Some(detail)) = serde_json::from_value::<Option<ErrorDetail>>(detail.clone()) else {
        return Some(malformed());
    };
    let mut code = detail.kind.trim().to_owned();
    if code.is_empty() {
        code = detail.code.trim().to_owned();
    }
    if code.is_empty() {
        return Some(malformed());
    }
    let message = match detail.message.trim() {
        "" => "home returned error".to_owned(),
        m => m.to_owned(),
    };
    let mut error = HomeError {
        code: code.clone(),
        message,
        retryable: detail.retryable,
        status: 502,
        kind: HomeErrorKind::Plain,
    };
    match code.to_lowercase().as_str() {
        "model_not_found" => error.status = 404,
        "model_cooldown" => {
            error.status = 429;
            error.kind = HomeErrorKind::Cooldown {
                retry_after: positive_millis(detail.retry_after_ms),
                request_retry: detail.request_retry.filter(|r| *r >= 0),
            };
        }
        "authentication_error" | "unauthorized" | "no_credentials" | "invalid_credential" => error.status = 401,
        "credential_concurrency_exceeded" | "credential_model_concurrency_exceeded" => {
            error.status = 429;
            error.kind = HomeErrorKind::Busy {
                retry_after: positive_millis(detail.retry_after_ms),
            };
        }
        "user_credits_insufficient" => error.status = 402,
        "user_period_limit_exceeded" => error.status = 429,
        "auth_not_found"
        | "auth_unavailable"
        | "refresh_temporarily_unavailable"
        | "home_unavailable"
        | "concurrency_protocol_required"
        | "concurrency_tracker_unavailable"
        | "concurrency_node_unavailable" => error.status = 503,
        _ => {}
    }
    Some(error)
}

/// Go `homeAuthDispatchResponse`. `auth` is Go's `coreauth.Auth` JSON.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DispatchResponse {
    pub model: String,
    pub provider: String,
    pub auth_index: String,
    pub user_api_key: String,
    pub request_retry: Option<i64>,
    pub force_mapping: bool,
    pub original_alias: String,
    pub model_info: Option<Value>,
    pub auth: Map<String, Value>,
}

impl DispatchResponse {
    /// Decodes a successful reply. Older Homes returned the auth object itself.
    pub fn parse(raw: &[u8]) -> Result<Self, String> {
        let mut response: DispatchResponse =
            serde_json::from_slice(raw).map_err(|_| "home returned invalid auth payload".to_owned())?;
        // Go decodes `model_info` into its typed struct: a mistyped field fails the reply.
        if response.model_info.as_ref().is_some_and(|info| !model_info_shape(info)) {
            return Err("home returned invalid auth payload".to_owned());
        }
        let auth_id = response.auth.get("id").and_then(Value::as_str).unwrap_or_default();
        if auth_id.trim().is_empty() {
            response.auth = serde_json::from_slice(raw).map_err(|_| "home returned invalid auth payload".to_owned())?;
        }
        Ok(response)
    }

    pub fn auth_id(&self) -> &str {
        self.auth.get("id").and_then(Value::as_str).unwrap_or_default()
    }

    /// Go `canonicalHomeDispatchModel`.
    pub fn observed_model<'a>(&'a self, requested: &'a str) -> &'a str {
        match self.model.trim() {
            "" => requested,
            model => model,
        }
    }
}

/// Whether a JSON value has the shape a Go field type accepts.
type Shape = fn(&Value) -> bool;

/// Go's JSON decoding into a typed struct accepts null for any field and ignores
/// unknown ones; `kinds` lists each known field with the shape it must have.
fn typed_fields(object: &Map<String, Value>, kinds: &[(&str, Shape)]) -> bool {
    kinds
        .iter()
        .all(|(key, shape)| object.get(*key).is_none_or(|v| v.is_null() || shape(v)))
}

fn is_int(v: &Value) -> bool {
    v.as_i64().is_some()
}

fn is_strings(v: &Value) -> bool {
    v.as_array()
        .is_some_and(|a| a.iter().all(|s| s.is_string() || s.is_null()))
}

/// Go `registry.ThinkingSupport`.
fn is_thinking(v: &Value) -> bool {
    v.as_object().is_some_and(|t| {
        typed_fields(
            t,
            &[
                ("min", is_int),
                ("max", is_int),
                ("zero_allowed", Value::is_boolean),
                ("dynamic_allowed", Value::is_boolean),
                ("levels", is_strings),
            ],
        )
    })
}

/// Go `homeDispatchModelInfo`.
fn model_info_shape(info: &Value) -> bool {
    info.is_null()
        || info.as_object().is_some_and(|m| {
            typed_fields(
                m,
                &[
                    ("id", Value::is_string),
                    ("type", Value::is_string),
                    ("inputTokenLimit", is_int),
                    ("outputTokenLimit", is_int),
                    ("context_length", is_int),
                    ("max_completion_tokens", is_int),
                    ("thinking", is_thinking),
                    (
                        "native_capabilities",
                        (|v: &Value| {
                            v.as_object()
                                .is_some_and(|n| typed_fields(n, &[("web_search", Value::is_boolean)]))
                        }) as fn(&Value) -> bool,
                    ),
                    ("support_configuration_update", Value::is_boolean),
                    ("user_defined", Value::is_boolean),
                ],
            )
        })
}

/// Go `config.OpenAICompatibilityModel`, as Go's JSON decoding accepts it (null, or an
/// object whose known fields have their types).
pub fn compat_model_shape(model: &Value) -> bool {
    model.is_null()
        || model.as_object().is_some_and(|m| {
            typed_fields(
                m,
                &[
                    ("name", Value::is_string),
                    ("alias", Value::is_string),
                    ("display-name", Value::is_string),
                    ("max-context-length", is_int),
                    ("force-mapping", Value::is_boolean),
                    ("image", Value::is_boolean),
                    ("input-modalities", is_strings),
                    ("output-modalities", is_strings),
                    ("is-compat", Value::is_boolean),
                    ("use-max-completion-tokens", Value::is_boolean),
                    ("thinking", is_thinking),
                ],
            )
        })
}

/// Go `homeAPIKeyModelOptions`: the `credential_options.models` entry Home mode uses for
/// `model` reached through `route_model`. `None` when the options list no models (absent,
/// not an array, or a mistyped entry); `Some` of an empty map when no entry matches.
/// An entry whose upstream name (alias when unnamed) matches the model, exactly or
/// suffix-free, and whose alias or name matches the route wins; then upstream names,
/// then aliases.
pub fn credential_model_options(
    credential: &cpa_core::credential::Credential,
    model: &str,
    route_model: &str,
) -> Option<Map<String, Value>> {
    use cpa_common::gostr::GoStr;
    let models = credential
        .metadata
        .get("credential_options")?
        .as_object()?
        .get("models")?;
    let models: &[Value] = match models {
        Value::Null => &[],
        Value::Array(models) if models.iter().all(compat_model_shape) => models,
        _ => return None,
    };
    let field = |m: &Value, key: &str| m.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_owned();
    let entry = |m: &Value| m.as_object().cloned().unwrap_or_default();
    let requested = model.trim();
    if requested.is_empty() {
        return Some(Map::new());
    }
    let base = cpa_common::thinking::parse_suffix(requested)
        .model_name
        .trim()
        .to_owned();
    let route = cpa_core::registry::dynamic::strip_prefix(route_model.trim(), credential);
    let routes: Vec<String> = if route.is_empty() {
        Vec::new()
    } else {
        let route_base = cpa_common::thinking::parse_suffix(route).model_name;
        let route_base = if route_base.is_empty() {
            route.to_owned()
        } else {
            route_base
        };
        if route_base == route {
            vec![route.to_owned()]
        } else {
            vec![route.to_owned(), route_base]
        }
    };
    for route in &routes {
        for candidate in [requested, base.as_str()] {
            for m in models {
                let (mut name, alias) = (field(m, "name"), field(m, "alias"));
                if name.is_empty() {
                    name = alias.clone();
                }
                if name.go_eq_fold(candidate) && (alias.go_eq_fold(route) || name.go_eq_fold(route)) {
                    return Some(entry(m));
                }
            }
        }
    }
    for use_alias in [false, true] {
        for candidate in [requested, base.as_str()] {
            if candidate.is_empty() {
                continue;
            }
            for m in models {
                let mut name = field(m, "name");
                if use_alias || name.is_empty() {
                    name = field(m, "alias");
                }
                if name.go_eq_fold(candidate) {
                    return Some(entry(m));
                }
            }
        }
    }
    Some(Map::new())
}

/// Go `verifyAccountedHomeConcurrencyIdentity`.
pub fn verify_identity(tuple: &ConcurrencyTuple, auth_id: &str, auth_index: &str) -> Result<(), String> {
    if tuple.accounted && (auth_id != tuple.credential_id || auth_index != tuple.credential_id) {
        return Err("Home concurrency identity does not match dispatched auth".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `TestCanonicalHomeConcurrencyModelKeyMatchesHomeLimiter` (Go's malformed
    /// UTF-8 input cannot reach a Rust `&str`; JSON decoding rejects it first, see
    /// `malformed_tuples_are_present_errors`).
    #[test]
    fn model_keys_drop_only_recognized_suffixes() {
        assert_eq!(canonical_concurrency_model_key(" GPT-5(High) "), "gpt-5");
        assert_eq!(canonical_concurrency_model_key("gpt-5(8192)"), "gpt-5");
        assert_eq!(canonical_concurrency_model_key("gpt-5(-1)"), "gpt-5");
        assert_eq!(
            canonical_concurrency_model_key("gpt-5(2147483648)"),
            "gpt-5(2147483648)"
        );
        assert_eq!(
            canonical_concurrency_model_key("gpt-5(12345678901)"),
            "gpt-5(12345678901)"
        );
        assert_eq!(canonical_concurrency_model_key("gpt-5(fast)"), "gpt-5(fast)");
        assert_eq!(canonical_concurrency_model_key("(high)"), "(high)");
        assert_eq!(canonical_concurrency_model_key("a (b) (low)"), "a (b)");
        assert_eq!(valid_concurrency_model_key("  "), None);
        assert_eq!(valid_concurrency_model_key(&"m".repeat(257)), None);
    }

    /// Go `TestConcurrencyDispatchFixture` (accounted): the shared fixture's tuple and
    /// identity; the pick side is `home::tests::picks_install_release_and_fence_like_go`.
    #[test]
    fn concurrency_fixture_round_trips() {
        let raw = include_bytes!("../tests/fixtures/concurrency_dispatch_accounted.json");
        let tuple = decode_concurrency(raw).unwrap().unwrap();
        assert_eq!(
            tuple,
            ConcurrencyTuple {
                accounted: true,
                credential_id: "cred-1".into(),
                model: "gpt".into()
            }
        );
        let response = DispatchResponse::parse(raw).unwrap();
        assert_eq!((response.auth_id(), response.auth_index.as_str()), ("cred-1", "cred-1"));
        verify_identity(&tuple, response.auth_id(), &response.auth_index).unwrap();
        assert!(verify_identity(&tuple, "cred-1", "other").is_err());
    }

    /// Go `TestAccountedHomeConcurrencyTupleRequiresCanonicalLimiterModel`,
    /// `TestInstallHomeConcurrencyScopeRejectsNonCanonicalTuple` and
    /// `TestHomeConcurrencyTupleStringsAreValidUTF8`: a present tuple that is not
    /// canonical is malformed (a fencing error); a body that is not UTF-8 JSON is not a
    /// tuple at all.
    #[test]
    fn malformed_tuples_are_present_errors() {
        for raw in [
            r#"{"concurrency":{"accounted":false,"credential_id":"c","model":"gpt"}}"#,
            r#"{"concurrency":{"accounted":true,"credential_id":" c","model":"gpt"}}"#,
            r#"{"concurrency":{"accounted":true,"credential_id":"c","model":"GPT"}}"#,
            r#"{"concurrency":{"accounted":true,"credential_id":"c","model":"gpt(high)"}}"#,
            r#"{"concurrency":null}"#,
            r#"{"concurrency":"x"}"#,
        ] {
            assert!(matches!(decode_concurrency(raw.as_bytes()), Err((true, _))), "{raw}");
        }
        assert!(matches!(decode_concurrency(b"[1]"), Err((false, _))));
        assert!(matches!(decode_concurrency(b"\xff"), Err((false, _))));
        assert_eq!(decode_concurrency(br#"{"auth":{}}"#), Ok(None));
    }

    /// Go `TestConcurrencyDispatchFixture` (busy), `TestHomeBusyErrorMaps429AndRetryAfter`,
    /// `TestHomeBusyErrorHeadersRoundUpMilliseconds` and
    /// `TestHomeConcurrencyBusyErrorsRemainTypedWhenWrapped`.
    #[test]
    fn busy_fixture_maps_to_429_with_retry_after() {
        let raw = include_bytes!("../tests/fixtures/concurrency_dispatch_busy.json");
        let error = decode_error(raw).unwrap();
        assert_eq!(error.code, "credential_concurrency_exceeded");
        assert_eq!(error.status, 429);
        assert!(error.retryable);
        assert_eq!(
            error.kind,
            HomeErrorKind::Busy {
                retry_after: Some(Duration::from_millis(750))
            }
        );
        assert_eq!(error.retry_after_header(), Some(1));
        // Both busy codes stay busy without a retry hint, and keep retryable as sent.
        for code in [
            "credential_concurrency_exceeded",
            "credential_model_concurrency_exceeded",
        ] {
            let raw = format!(r#"{{"error":{{"type":"{code}","message":"busy","retryable":false}}}}"#);
            let error = decode_error(raw.as_bytes()).unwrap();
            assert_eq!(error.kind, HomeErrorKind::Busy { retry_after: None }, "{code}");
            assert_eq!((error.code.as_str(), error.retryable), (code, false));
        }
    }

    /// Go `TestHomeNoCandidateErrorsMapToServiceUnavailable` and
    /// `TestHomeUserBillingAndPeriodLimitErrors`.
    #[test]
    fn error_codes_map_to_go_statuses() {
        let status = |code: &str| {
            decode_error(format!(r#"{{"error":{{"type":"{code}"}}}}"#).as_bytes())
                .unwrap()
                .status
        };
        assert_eq!(status("auth_not_found"), 503);
        assert_eq!(status("auth_unavailable"), 503);
        assert_eq!(status("model_not_found"), 404);
        assert_eq!(status("MODEL_COOLDOWN"), 429);
        assert_eq!(status("no_credentials"), 401);
        assert_eq!(status("user_credits_insufficient"), 402);
        assert_eq!(status("user_period_limit_exceeded"), 429);
        assert_eq!(status("concurrency_node_unavailable"), 503);
        assert_eq!(status("something_else"), 502);
        let cooldown =
            decode_error(br#"{"error":{"code":"model_cooldown","retry_after_ms":1500,"request_retry":2}}"#).unwrap();
        assert_eq!(cooldown.message, "home returned error");
        assert_eq!(
            cooldown.kind,
            HomeErrorKind::Cooldown {
                retry_after: Some(Duration::from_millis(1500)),
                request_retry: Some(2)
            }
        );
        assert_eq!(cooldown.retry_after_header(), Some(2));
        for malformed in [r#"{"error":null}"#, r#"{"error":"x"}"#, r#"{"error":{"message":"m"}}"#] {
            let error = decode_error(malformed.as_bytes()).unwrap();
            assert_eq!(
                (error.code.as_str(), error.status),
                ("invalid_auth", 502),
                "{malformed}"
            );
        }
        assert!(decode_error(br#"{"auth":{}}"#).is_none());
        assert!(decode_error(b"not json").is_none());
    }

    #[test]
    fn legacy_replies_carry_the_auth_at_top_level() {
        let legacy = DispatchResponse::parse(br#"{"id":"a1","provider":"claude"}"#).unwrap();
        assert_eq!(legacy.auth_id(), "a1");
        assert_eq!(legacy.observed_model("req"), "req");
        assert!(DispatchResponse::parse(b"[]").is_err());
    }

    #[test]
    fn retry_after_rounds_up_to_whole_seconds() {
        assert_eq!(retry_after_seconds(Duration::from_millis(1)), Some(1));
        assert_eq!(retry_after_seconds(Duration::from_millis(1000)), Some(1));
        assert_eq!(retry_after_seconds(Duration::from_millis(1001)), Some(2));
        assert_eq!(retry_after_seconds(Duration::ZERO), None);
    }
}

#[cfg(test)]
mod go_golden {
    use super::*;
    use serde_json::Value as Json;

    fn doc() -> Json {
        serde_json::from_str(include_str!("../tests/fixtures/go_auth_golden.json")).unwrap()
    }

    #[test]
    fn dispatch_errors_match_go() {
        for case in doc()["dispatch_errors"].as_array().unwrap() {
            let raw = case["raw"].as_str().unwrap();
            let got = decode_error(raw.as_bytes());
            let Some(code) = case["code"].as_str() else {
                assert!(got.is_none(), "{raw}");
                continue;
            };
            let got = got.unwrap_or_else(|| panic!("{raw}"));
            assert_eq!(got.code, code, "{raw}");
            assert_eq!(got.message, case["message"].as_str().unwrap(), "{raw}");
            assert_eq!(i64::from(got.status), case["status"].as_i64().unwrap(), "{raw}");
            assert_eq!(got.retryable, case["retryable"].as_bool().unwrap(), "{raw}");
            let ms = |d: Option<Duration>| d.map(|d| d.as_millis() as i64);
            let (kind, retry_after, request_retry) = match got.kind {
                HomeErrorKind::Plain => ("plain", None, None),
                HomeErrorKind::Busy { retry_after } => ("busy", ms(retry_after), None),
                HomeErrorKind::Cooldown {
                    retry_after,
                    request_retry,
                } => ("cooldown", ms(retry_after), request_retry),
            };
            assert_eq!(kind, case["kind"].as_str().unwrap(), "{raw}");
            assert_eq!(retry_after, case["retry_after_ms"].as_i64(), "{raw}");
            assert_eq!(request_retry, case["request_retry"].as_i64(), "{raw}");
            let header = got.retry_after_header().map(|s| s.to_string());
            assert_eq!(header.as_deref(), case["retry_after_header"].as_str(), "{raw}");
        }
    }

    #[test]
    fn model_keys_match_go() {
        for case in doc()["model_keys"].as_array().unwrap() {
            let input = case["in"].as_str().unwrap();
            assert_eq!(
                canonical_concurrency_model_key(input),
                case["key"].as_str().unwrap(),
                "{input:?}"
            );
            assert_eq!(
                valid_concurrency_model_key(input).is_some(),
                case["valid"].as_bool().unwrap(),
                "{input:?}"
            );
        }
    }

    #[test]
    fn envelopes_match_go() {
        for case in doc()["envelopes"].as_array().unwrap() {
            let raw = case["raw"].as_str().unwrap();
            let got = decode_concurrency(raw.as_bytes());
            let (ok, present) = (case["ok"].as_bool().unwrap(), case["present"].as_bool().unwrap());
            match got {
                Ok(tuple) => {
                    assert!(ok, "{raw}");
                    assert_eq!(tuple.is_some(), present, "{raw}");
                    if let Some(tuple) = tuple {
                        let want = &case["tuple"];
                        assert_eq!(tuple.credential_id, want["credential_id"].as_str().unwrap());
                        assert_eq!(tuple.model, want["model"].as_str().unwrap());
                    }
                }
                Err((got_present, _)) => {
                    assert!(!ok, "{raw}");
                    assert_eq!(got_present, present, "{raw}");
                }
            }
        }
    }
}
