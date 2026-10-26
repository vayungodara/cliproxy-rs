//! Go d9451cb6 `helps/vertex_openapi.go`: partner and open (MaaS) models, and
//! credentials that opt in with `openapi`, reach Vertex's OpenAI-compatible
//! `endpoints/openapi` route.

use cpa_common::thinking::parse_suffix;
use cpa_core::credential::Credential;
use cpa_core::gostr::GoStr;
use serde_json::Value;

const DEFAULT_LOCATION: &str = "us-central1";

/// `strings.HasPrefix(strings.ToLower(s), prefix)` then `s[len(prefix):]`, matching
/// ASCII case variants only. Go also lowercases `İ` (U+0130, two bytes) to `i`, so it
/// matches `publİshers/<p>/models/<m>`, but then slices the original one byte short:
/// the rest starts with `/`, the publisher is empty and the cut fails, as here.
fn strip_prefix_lower<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// `strings.Cut` with both halves trimmed.
fn cut_trimmed(s: &str) -> Option<(&str, &str)> {
    s.split_once('/').map(|(a, b)| (a.trim(), b.trim()))
}

/// `cutVertexPublisherResource`: `publishers/<publisher>/models/<id>`.
fn cut_publisher_resource(base: &str) -> Option<(&str, &str)> {
    let rest = strip_prefix_lower(base, "publishers/")?.trim();
    let (publisher, tail) = cut_trimmed(rest)?;
    if publisher.is_empty() || tail.is_empty() {
        return None;
    }
    let id = strip_prefix_lower(tail, "models/")?.trim();
    (!id.is_empty()).then_some((publisher, id))
}

/// `IsVertexOpenAPIModel`: `<publisher>/<model>` (or its publisher resource) outside
/// Google.
pub(crate) fn is_openapi_model(model: &str) -> bool {
    let base = parse_suffix(model).model_name;
    let base = base.trim();
    if base.is_empty() {
        return false;
    }
    if let Some((publisher, id)) = cut_publisher_resource(base) {
        return !publisher.go_eq_fold("google") && !id.contains('/');
    }
    let Some((publisher, id)) = cut_trimmed(base) else {
        return false;
    };
    if publisher.is_empty() || id.is_empty() || id.contains('/') {
        return false;
    }
    !["google", "models", "publishers"]
        .iter()
        .any(|p| publisher.go_eq_fold(p))
}

/// `NormalizeVertexOpenAPIModel`: `<publisher>/<model>`, keeping a thinking suffix.
pub(crate) fn normalize_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let parsed = parse_suffix(trimmed);
    let base = match parsed.model_name.trim() {
        "" => trimmed,
        base => base,
    };
    let normalized = normalize_base(base);
    if parsed.has_suffix {
        format!("{normalized}({})", parsed.raw_suffix)
    } else {
        normalized
    }
}

fn normalize_base(base: &str) -> String {
    if let Some((publisher, id)) = cut_publisher_resource(base) {
        return format!("{}/{id}", publisher.go_lower());
    }
    if let Some(id) = strip_prefix_lower(base, "models/").map(str::trim)
        && !id.is_empty()
    {
        return format!("google/{id}");
    }
    if let Some((publisher, id)) = cut_trimmed(base)
        && !publisher.is_empty()
        && !id.is_empty()
    {
        return format!("{publisher}/{id}");
    }
    format!("google/{base}")
}

/// `parseVertexOpenAPIBool`.
fn parse_bool(raw: Option<&Value>) -> Option<bool> {
    match raw? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().go_lower().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `ShouldExecuteVertexOpenAPI`: an explicit `openapi` setting (attribute, then
/// metadata) wins, except for Imagen models; otherwise partner and open models.
pub(crate) fn should_execute(credential: &Credential, model: &str) -> bool {
    let attr = credential.attributes.get("openapi").map(|v| Value::String(v.clone()));
    let explicit = parse_bool(attr.as_ref()).or_else(|| parse_bool(credential.metadata.get("openapi")));
    match explicit {
        Some(false) => false,
        Some(true) => !parse_suffix(model).model_name.trim().go_lower().contains("imagen"),
        None => is_openapi_model(model),
    }
}

/// `IsGoogleVertexHost`: empty, or Google's aiplatform.googleapis.com domain.
pub(crate) fn is_google_host(base: &str) -> bool {
    let trimmed = base.trim();
    if trimmed.is_empty() {
        return true;
    }
    match crate::xai_url::parse(trimmed) {
        Ok(url) if !url.host.is_empty() => {
            let host = url.hostname.go_lower();
            host == "aiplatform.googleapis.com"
                || host.ends_with("-aiplatform.googleapis.com")
                || host.ends_with(".aiplatform.googleapis.com")
        }
        _ => trimmed.go_lower().contains("aiplatform.googleapis.com"),
    }
}

fn metadata_str<'a>(credential: &'a Credential, key: &str) -> Option<&'a str> {
    credential.str(key).map(str::trim).filter(|v| !v.is_empty())
}

/// `VertexOpenAPILocation`: an `openapi_location` override, else the credential's
/// location when no default is given, else the default (us-central1 when empty).
pub(crate) fn location(credential: &Credential, default: &str) -> String {
    let no_default = default.trim().is_empty();
    for key in ["openapi_location", "openapi-location", "location"] {
        if let Some(v) = credential
            .attributes
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            && (key != "location" || no_default)
        {
            return v.to_owned();
        }
    }
    let from_metadata = ["openapi_location", "openapi-location"]
        .into_iter()
        .find_map(|key| metadata_str(credential, key))
        .or_else(|| no_default.then(|| metadata_str(credential, "location")).flatten());
    if let Some(v) = from_metadata {
        return v.to_owned();
    }
    if no_default {
        DEFAULT_LOCATION.into()
    } else {
        default.trim().to_owned()
    }
}

/// `VertexAuthBaseURL`: the `base_url` attribute, else the file's `base_url` /
/// `base-url`.
pub(crate) fn auth_base_url(credential: &Credential) -> String {
    if let Some(v) = credential
        .attributes
        .get("base_url")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        return v.to_owned();
    }
    ["base_url", "base-url"]
        .into_iter()
        .find_map(|key| metadata_str(credential, key))
        .unwrap_or_default()
        .to_owned()
}

/// `VertexAuthProjectID`: the file's `project_id`, else `project`.
pub(crate) fn auth_project_id(credential: &Credential) -> String {
    ["project_id", "project"]
        .into_iter()
        .find_map(|key| metadata_str(credential, key))
        .unwrap_or_default()
        .to_owned()
}

/// `VertexOpenAPIHostBaseURL`.
fn host_base_url(location: &str) -> String {
    match location.trim() {
        "" => format!("https://{DEFAULT_LOCATION}-aiplatform.googleapis.com"),
        loc if loc.go_eq_fold("global") => "https://aiplatform.googleapis.com".into(),
        loc => format!("https://{loc}-aiplatform.googleapis.com"),
    }
}

/// The steps both builders share: a base that already names the route, a publishers
/// prefix, or a project and location.
fn existing_route(base: &str) -> Option<String> {
    if base.ends_with("/endpoints/openapi") {
        return Some(base.to_owned());
    }
    if let Some(idx) = base.find("/publishers/") {
        return Some(format!("{}/endpoints/openapi", &base[..idx]));
    }
    (base.contains("/projects/") && base.contains("/locations/")).then(|| format!("{base}/endpoints/openapi"))
}

fn or_v1(version: &str) -> &str {
    match version.trim() {
        "" => "v1",
        v => v,
    }
}

/// `VertexServiceAccountOpenAPIBaseURL`:
/// `<base>/<version>/projects/<project>/locations/<location>/endpoints/openapi`.
pub(crate) fn service_account_base_url(base: &str, version: &str, project: &str, location: &str) -> String {
    let location = match location.trim() {
        "" => DEFAULT_LOCATION,
        loc => loc,
    };
    let mut base = base.trim().trim_end_matches('/').to_owned();
    if base.is_empty() {
        base = host_base_url(location);
    }
    if let Some(route) = existing_route(&base) {
        return route;
    }
    if !has_version_segment(&base) {
        base = format!("{base}/{}", or_v1(version));
    }
    format!(
        "{base}/projects/{}/locations/{location}/endpoints/openapi",
        project.trim()
    )
}

/// `VertexAPIKeyOpenAPIBaseURL`: a third-party Vertex-compatible relay.
pub(crate) fn api_key_base_url(base: &str, version: &str, project: &str, location: &str) -> String {
    let version = or_v1(version);
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return String::new();
    }
    if let Some(route) = existing_route(base) {
        return route;
    }
    if !project.trim().is_empty() {
        return service_account_base_url(base, version, project, location);
    }
    if has_version_segment(base) {
        format!("{base}/endpoints/openapi")
    } else {
        format!("{base}/{version}/endpoints/openapi")
    }
}

/// `vertexOpenAPIHasVersionSegment`: `/v1/` or `/v1beta1/` anywhere, or a last segment
/// of `v`, then lower-case letters, digits, `_` or `-`, with at least one digit.
fn has_version_segment(base: &str) -> bool {
    if base.contains("/v1/") || base.contains("/v1beta1/") {
        return true;
    }
    let seg = &base[base.rfind('/').map_or(0, |i| i + 1)..];
    let Some(rest) = seg.strip_prefix('v').filter(|r| !r.is_empty()) else {
        return false;
    };
    rest.bytes()
        .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || c == b'_' || c == b'-')
        && rest.bytes().any(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go helps/vertex_openapi_test.go.
    #[test]
    fn openapi_models_match_go() {
        for (model, want) in [
            ("deepseek-ai/deepseek-v4.1-flash-maas", true),
            ("deepseek-ai/deepseek-v4.1-flash-maas(high)", true),
            ("qwen/qwen3-coder-480b-a35b-instruct-maas", true),
            ("xai/grok-4-maas", true),
            ("publishers/deepseek-ai/models/deepseek-v4.1-flash-maas", true),
            ("publishers/google/models/gemini-2.5-pro", false),
            ("publishers/deepseek-ai/deepseek-v4.1-flash-maas", false),
            ("deepseek-ai/extra/segment", false),
            ("google/gemini-2.5-pro", false),
            ("models/gemini-2.5-pro", false),
            ("gemini-2.5-pro", false),
            ("imagen-3.0-generate-002", false),
            ("", false),
            // Go's EqualFold: GOOGLE and the long s of ſ fold alike.
            ("GOOGLE/gemini", false),
            ("Publishers/Meta/Models/llama", true),
        ] {
            assert_eq!(is_openapi_model(model), want, "{model}");
        }
        for (model, want) in [
            (
                "deepseek-ai/deepseek-v4.1-flash-maas",
                "deepseek-ai/deepseek-v4.1-flash-maas",
            ),
            (
                "publishers/deepseek-ai/models/deepseek-v4.1-flash-maas",
                "deepseek-ai/deepseek-v4.1-flash-maas",
            ),
            (
                "publishers/deepseek-ai/models/deepseek-v4.1-flash-maas(high)",
                "deepseek-ai/deepseek-v4.1-flash-maas(high)",
            ),
            ("gemini-2.5-pro", "google/gemini-2.5-pro"),
            ("gemini-2.5-pro(8192)", "google/gemini-2.5-pro(8192)"),
            ("models/gemini-2.5-pro", "google/gemini-2.5-pro"),
            ("publishers/google/models/gemini-2.5-pro", "google/gemini-2.5-pro"),
            ("PUBLISHERS/Meta/MODELS/Llama", "meta/Llama"),
            ("", ""),
        ] {
            assert_eq!(normalize_model(model), want, "{model}");
        }
    }

    fn credential(attributes: &[(&str, &str)], metadata: Value) -> Credential {
        let mut file = serde_json::json!({"type": "vertex"});
        for (k, v) in metadata.as_object().into_iter().flatten() {
            file[k] = v.clone();
        }
        let mut c = Credential::from_file(
            std::path::Path::new("/fixture"),
            std::path::Path::new("/fixture/vertex.json"),
            file.as_object().unwrap().clone(),
        )
        .unwrap();
        for (k, v) in attributes {
            c.attributes.insert((*k).to_owned(), (*v).to_owned());
        }
        c
    }

    #[test]
    fn explicit_settings_match_go() {
        let none = credential(&[], Value::Null);
        assert!(should_execute(&none, "deepseek-ai/deepseek-v4.1-flash-maas"));
        assert!(!should_execute(&none, "gemini-2.5-pro"));
        let off = credential(&[], serde_json::json!({"openapi": false}));
        assert!(!should_execute(&off, "deepseek-ai/deepseek-v4.1-flash-maas"));
        let on = credential(&[("openapi", "true")], Value::Null);
        assert!(should_execute(&on, "gemini-2.5-pro"));
        assert!(!should_execute(&on, "Imagen-4.0(high)"));
        // The attribute wins over the file; an unparsable attribute falls through.
        let both = credential(&[("openapi", " 0 ")], serde_json::json!({"openapi": true}));
        assert!(!should_execute(&both, "deepseek-ai/x"));
        let junk = credential(&[("openapi", "yes")], serde_json::json!({"openapi": "1"}));
        assert!(should_execute(&junk, "gemini-2.5-pro"));
    }

    #[test]
    fn base_urls_match_go() {
        assert_eq!(
            service_account_base_url("", "v1", "my-proj", "global"),
            "https://aiplatform.googleapis.com/v1/projects/my-proj/locations/global/endpoints/openapi"
        );
        assert_eq!(
            service_account_base_url("", "v1", "my-proj", "us-central1"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/endpoints/openapi"
        );
        assert_eq!(
            api_key_base_url("https://example.com/v1/projects/my-proj/locations/global", "v1", "", ""),
            "https://example.com/v1/projects/my-proj/locations/global/endpoints/openapi"
        );
        assert_eq!(
            api_key_base_url("https://example.com/api", "v1", "", ""),
            "https://example.com/api/v1/endpoints/openapi"
        );
        assert_eq!(
            api_key_base_url("https://example.com/api/", "v1", "p", ""),
            "https://example.com/api/v1/projects/p/locations/us-central1/endpoints/openapi"
        );
        assert_eq!(api_key_base_url(" ", "v1", "p", "x"), "");
        assert!(is_google_host(""));
        assert!(is_google_host("https://aiplatform.googleapis.com/v1"));
        assert!(is_google_host("https://us-central1-aiplatform.googleapis.com"));
        assert!(is_google_host("aiplatform.googleapis.com/v1"));
        assert!(is_google_host("https://AIPlatform.googleapis.com:443"));
        assert!(!is_google_host("https://zenmux.ai/api"));
        assert!(!is_google_host("https://aiplatform.googleapis.com.evil.example"));
    }

    #[test]
    fn locations_match_go() {
        let file = credential(&[], serde_json::json!({"location": "europe-west4"}));
        assert_eq!(location(&file, ""), "europe-west4");
        assert_eq!(location(&file, "global"), "global");
        let attr = credential(&[("location", "asia-east1")], Value::Null);
        assert_eq!(location(&attr, "global"), "global");
        assert_eq!(location(&attr, ""), "asia-east1");
        let over = credential(
            &[],
            serde_json::json!({"openapi-location": " us-east5 ", "location": "x"}),
        );
        assert_eq!(location(&over, "global"), "us-east5");
        assert_eq!(location(&credential(&[], Value::Null), " "), "us-central1");
    }
}
