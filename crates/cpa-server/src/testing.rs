//! Test support: keeps a test runtime off the network beyond loopback.
//!
//! Tests must never reach a provider, even when a test credential has no local base URL
//! and its provider gains an executor later. A runtime built by [`runtime`] hands each
//! executor call (execute, session turns, prepare and refresh) a [`guarded`] copy of the
//! credential: one that is not local is routed to a dead loopback proxy through the
//! production rule every executor follows (`Proxy::effective`, where the credential's
//! own `proxy_url` wins), so it fails with a connection error instead of leaving the
//! machine. A credential is local when its base URL or its own proxy is loopback, or
//! the test marked it [`local`] because the test built its executor against a mock.
//! A non-loopback proxy (including `direct`) does not count: it could reach a
//! provider. Stored credentials never change, so management listings, reconciliation and
//! features that pick their own proxy (management `api-call`, `latest-version`) behave
//! as configured.
// ponytail: the realtime routes dial through the Codex executor's live endpoints
// directly, outside these executor calls; their tests build those endpoints on mocks.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;

use crate::Runtime;

/// The deny-all proxy: the discard port on loopback, where nothing listens.
pub const DENY_PROXY: &str = "http://127.0.0.1:9";

/// Attribute set by [`local`].
const LOCAL: &str = "cpa_test_local";

/// [`Runtime::new`] whose executor calls cannot leave the machine.
pub fn runtime(config: Config, credentials: Vec<Credential>, executors: Executors) -> Runtime {
    let rt = Runtime::new(config, credentials, executors);
    rt.deny_external.store(true, Ordering::Relaxed);
    rt
}

/// Marks a credential whose executor the test pointed at a local mock (for example
/// `ClaudeExecutor::new(mock_url)`), so executor calls use it as stored.
pub fn local(mut credential: Credential) -> Credential {
    credential.attributes.insert(LOCAL.into(), "true".into());
    credential
}

/// `credential` itself when it is local, otherwise a copy whose `proxy_url` is
/// [`DENY_PROXY`]. A loopback base URL behind a proxy that is not loopback (its own or
/// `requests.proxy-url`) gets a `direct` copy, so the mock is reached without the
/// request ever going to that proxy. Only the credential's own loopback proxy exempts
/// it: a loopback `requests.proxy-url` may be a real local proxy that forwards out.
pub fn guarded(credential: &Arc<Credential>, cfg: &Config) -> Arc<Credential> {
    let text = |key: &str| {
        credential
            .attributes
            .get(key)
            .map(String::as_str)
            .or_else(|| credential.str(key))
            .unwrap_or_default()
            .trim()
    };
    if credential.attributes.contains_key(LOCAL) || is_loopback(text("proxy_url")) {
        return credential.clone();
    }
    let replacement = if is_loopback(text("base_url")) {
        let proxy = cpa_exec::proxy::Proxy::effective_url(credential, cfg);
        if is_loopback(&proxy) || cpa_exec::proxy::Proxy::parse(&proxy) == cpa_exec::proxy::Proxy::Direct {
            return credential.clone();
        }
        "direct"
    } else {
        DENY_PROXY
    };
    let mut copy = Credential::clone(credential);
    copy.attributes.insert("proxy_url".into(), replacement.into());
    Arc::new(copy)
}

fn is_loopback(raw: &str) -> bool {
    let Ok(uri) = raw.parse::<axum::http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_exec::proxy::Proxy;

    fn credential(attrs: &[(&str, &str)], meta: serde_json::Value) -> Arc<Credential> {
        let mut metadata = meta.as_object().unwrap().clone();
        metadata.insert("type".into(), "claude".into());
        let mut c = Credential::from_file(
            std::path::Path::new("/fake"),
            std::path::Path::new("/fake/a.json"),
            metadata,
        )
        .unwrap();
        for (k, v) in attrs {
            c.attributes.insert((*k).into(), (*v).into());
        }
        Arc::new(c)
    }

    #[test]
    fn only_local_credentials_dial_out() {
        let cfg = Config::parse("requests:\n  proxy-url: http://127.0.0.1:7\n").unwrap();
        let creds = [
            credential(&[], serde_json::json!({})),
            credential(&[("base_url", "http://127.0.0.1:4100")], serde_json::json!({})),
            credential(&[], serde_json::json!({"base_url": "http://localhost:1/v1"})),
            credential(&[("base_url", "http://[::1]:2")], serde_json::json!({})),
            credential(&[("base_url", "https://api.x.ai")], serde_json::json!({})),
            credential(&[("base_url", "http://127.0.0.1.example.com")], serde_json::json!({})),
            credential(&[("proxy_url", "direct")], serde_json::json!({})),
            credential(&[("proxy_url", "http://127.0.0.1:3128")], serde_json::json!({})),
            Arc::new(local(Credential::clone(&credential(&[], serde_json::json!({}))))),
        ];
        let deny = Proxy::Url(DENY_PROXY.into());
        let global = Proxy::Url("http://127.0.0.1:7".into());
        let got: Vec<Proxy> = creds
            .iter()
            .map(|c| Proxy::effective(&guarded(c, &cfg), &cfg))
            .collect();
        assert_eq!(
            got,
            [
                deny.clone(),
                global.clone(),
                global.clone(),
                global.clone(),
                deny.clone(),
                deny.clone(),
                deny,
                Proxy::Url("http://127.0.0.1:3128".into()),
                global,
            ]
        );
        // The stored credential is untouched.
        assert!(!creds[0].attributes.contains_key("proxy_url"));

        // A loopback mock behind a proxy that is not loopback is reached directly.
        let outside = Config::parse("requests:\n  proxy-url: http://proxy.example:3128\n").unwrap();
        let mock = credential(&[("base_url", "http://127.0.0.1:4100")], serde_json::json!({}));
        assert_eq!(Proxy::effective(&guarded(&mock, &outside), &outside), Proxy::Direct);
        let own = credential(
            &[
                ("base_url", "http://127.0.0.1:4100"),
                ("proxy_url", "http://proxy.example:1"),
            ],
            serde_json::json!({}),
        );
        assert_eq!(Proxy::effective(&guarded(&own, &cfg), &cfg), Proxy::Direct);
        let elsewhere = credential(&[("base_url", "https://api.x.ai")], serde_json::json!({}));
        assert_eq!(Proxy::effective(&guarded(&elsewhere, &outside), &outside), deny_proxy());
    }

    fn deny_proxy() -> Proxy {
        Proxy::Url(DENY_PROXY.into())
    }

    /// A guarded credential fails with a connection error at the dead proxy; it never
    /// reaches its provider.
    #[tokio::test]
    async fn guarded_credential_never_leaves_the_machine() {
        let cfg = Config::parse("{}\n").unwrap();
        let guarded = guarded(&credential(&[], serde_json::json!({})), &cfg);
        let client = cpa_exec::proxy::GoClients::new(Default::default()).get(&Proxy::effective(&guarded, &cfg));
        let err = client
            .get("https://provider.invalid/v1/models")
            .send()
            .await
            .unwrap_err();
        // Refused at the proxy, not a name lookup of the target.
        assert!(format!("{err:?}").contains("Connection refused"), "{err:?}");
    }

    /// Only a runtime built by [`runtime`] guards executor calls.
    #[test]
    fn only_test_runtimes_guard() {
        let executors = || Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let c = credential(&[], serde_json::json!({}));
        let plain = Runtime::new(Config::default(), Vec::new(), executors());
        assert!(Arc::ptr_eq(&plain.for_executor(&c), &c));
        let test = runtime(Config::default(), Vec::new(), executors());
        assert_eq!(test.for_executor(&c).attributes["proxy_url"], DENY_PROXY);
    }
}
