//! Replays outputs recorded from Go's discovery code at 6fecc6e
//! (tests/reference/discovery -> tests/fixtures/discovery_*.json).
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use serde_json::Value;

use super::cli::{self, Options};
use super::iface::{self, Iface};
use super::*;

fn fixture(name: &str) -> Value {
    let text = match name {
        "discovery" => include_str!("../../tests/fixtures/discovery_go.json"),
        "cmd" => include_str!("../../tests/fixtures/discovery_cmd_go.json"),
        _ => include_str!("../../tests/fixtures/discovery_main_go.json"),
    };
    serde_json::from_str(text).expect("fixture JSON")
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

fn list(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().map(s).collect()).unwrap_or_default()
}

fn ips(v: &Value) -> Vec<IpAddr> {
    list(v)
        .iter()
        .map(|ip| ip.parse::<IpAddr>().unwrap().to_canonical())
        .collect()
}

#[test]
fn txt_records_match_go() {
    let cases = fixture("discovery")["txt"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 7);
    for case in cases {
        let o = &case["opts"];
        let opts = TxtOptions {
            version: s(&o["Version"]),
            product: s(&o["Product"]),
            protocols: list(&o["Protocols"]),
            features: list(&o["Features"]),
            api_path_openai: s(&o["APIPathOpenAI"]),
            api_path_anthropic: s(&o["APIPathAnthropic"]),
            api_path_gemini: s(&o["APIPathGemini"]),
            tls: o["TLS"].as_bool().unwrap(),
            auth_required: o["AuthRequired"].as_bool().unwrap(),
            auth_methods: list(&o["AuthMethods"]),
            instance_id: s(&o["InstanceID"]),
            node_role: s(&o["NodeRole"]),
            advertise_management: o["AdvertiseManagement"].as_bool().unwrap(),
        };
        assert_eq!(build_txt_records(&opts), list(&case["records"]), "{o}");
    }
}

#[test]
fn txt_parsing_names_subtypes_and_service_types_match_go() {
    let f = fixture("discovery");
    for case in f["parse_txt"].as_array().unwrap() {
        let want: BTreeMap<String, String> = serde_json::from_value(case["out"].clone()).unwrap();
        assert_eq!(parse_txt_records(&list(&case["in"])), want);
    }
    for case in f["instance_names"].as_array().unwrap() {
        assert_eq!(
            format_instance_name(&s(&case["name"]), &s(&case["id"])),
            s(&case["out"]),
            "{case}"
        );
    }
    for case in f["subtypes"].as_array().unwrap() {
        assert_eq!(sanitize_subtype(&s(&case["in"])), s(&case["out"]), "{case}");
    }
    for case in f["service_types"].as_array().unwrap() {
        let got = validate_service_type(&s(&case["in"])).err().unwrap_or_default();
        assert_eq!(got, s(&case["out"]), "{case}");
    }
    for case in f["sanitize_instance_name"].as_array().unwrap() {
        assert_eq!(sanitize_instance_name(&s(&case["in"])), s(&case["out"]));
    }
    for case in f["txt_lists"].as_array().unwrap() {
        assert_eq!(parse_txt_list(&s(&case["in"])), list(&case["out"]), "{case}");
    }
    for case in f["endpoint_paths"].as_array().unwrap() {
        assert_eq!(sanitize_endpoint_path(&s(&case["in"])), s(&case["out"]), "{case}");
    }
}

#[test]
fn interface_name_rules_match_go() {
    for case in fixture("discovery")["interfaces"].as_array().unwrap() {
        let name = s(&case["name"]);
        assert_eq!(
            iface::is_virtual_or_tunnel(&name),
            case["virtual"].as_bool().unwrap(),
            "{case}"
        );
        assert_eq!(
            iface::is_likely_physical_lan(&name),
            case["physical"].as_bool().unwrap(),
            "{case}"
        );
        assert_eq!(
            iface::matches_any(&name, &list(&case["include"])),
            case["in_include"].as_bool().unwrap(),
            "{case}"
        );
        assert_eq!(
            iface::matches_any(&name, &list(&case["exclude"])),
            case["in_exclude"].as_bool().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn interface_filter_applies_flags_names_and_addresses() {
    let iface = |index, name: &str, multicast, addrs: &[&str]| Iface {
        index,
        name: name.into(),
        up: true,
        loopback: name == "lo",
        point_to_point: name.starts_with("ppp"),
        multicast,
        addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
    };
    let all = vec![
        iface(1, "lo", true, &["127.0.0.1"]),
        iface(2, "eth0", true, &["192.0.2.2", "fe80::2"]),
        iface(3, "docker0", true, &["172.17.0.1"]),
        iface(4, "ETH1", true, &["0.0.0.0"]),
        iface(5, "wlan0", false, &["192.0.2.5"]),
        iface(6, "ppp0", true, &["192.0.2.6"]),
        iface(7, "en0", true, &["::1", "2001:db8::7"]),
    ];
    let names = |v: Vec<Iface>| v.into_iter().map(|i| i.name).collect::<Vec<_>>();
    assert_eq!(names(iface::filter_from(all.clone(), &[], &[])), ["eth0", "en0"]);
    assert_eq!(
        names(iface::filter_from(all.clone(), &["docker*".into()], &[])),
        ["docker0"]
    );
    assert_eq!(names(iface::filter_from(all.clone(), &[], &["EN0".into()])), ["eth0"]);
    let mut down = all.clone();
    down[1].up = false;
    assert_eq!(names(iface::filter_from(down, &[], &[])), ["en0"]);
}

#[test]
fn discovered_entries_and_merges_match_go() {
    let f = fixture("discovery");
    let mut outs = Vec::new();
    for case in f["entries"].as_array().unwrap() {
        let e = Entry {
            instance: s(&case["instance"]),
            service: s(&case["service"]),
            domain: s(&case["domain"]),
            host: s(&case["host"]),
            port: case["port"].as_i64().unwrap(),
            ipv4: ips(&case["ipv4"]),
            ipv6: ips(&case["ipv6"]),
            text: list(&case["text"]),
            nil_addr: false,
        };
        assert_eq!(
            entry_within_limits(&e),
            case["within_limits"].as_bool().unwrap(),
            "{case}"
        );
        let got = entry_to_discovered(&e);
        assert_eq!(got.to_json(), case["out"], "{case}");
        outs.push(got);
    }
    let (mut a, mut b) = (outs[0].clone(), outs[1].clone());
    b.instance_name.clone_from(&a.instance_name);
    b.host.clear();
    b.raw_txt.insert("auth_required".into(), "false".into());
    merge_discovered(&mut a, &b);
    assert_eq!(a.to_json(), f["merged"]);
}

#[test]
fn spec_errors_match_go() {
    let f = fixture("discovery");
    let cases = f["spec_errors"].as_array().unwrap();
    for case in &cases[..2] {
        let cfg = cpa_core::config::Config::parse(&s(&case["in"])).unwrap();
        let d = DiscoveryConfig::from_document(&cfg.document);
        assert!(d.enabled);
        let got = build_service_spec(&d, "", 8317, false, || "8F3B".into(), iface::filter).unwrap_err();
        assert_eq!(got, s(&case["out"]));
    }
    let d = DiscoveryConfig::default();
    let got = build_service_spec(&d, "", 0, false, || unreachable!(), iface::filter).unwrap_err();
    assert_eq!(got, s(&cases[2]["out"]));
}

fn lan(addrs: &[&str]) -> impl FnOnce(&[String], &[String]) -> Result<Vec<Iface>, String> {
    let iface = Iface {
        index: 2,
        name: "eth0".into(),
        up: true,
        loopback: false,
        point_to_point: false,
        multicast: true,
        addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
    };
    move |_, _| Ok(vec![iface])
}

#[test]
fn service_spec_follows_config_bind_host_and_tls() {
    let cfg = cpa_core::config::Config::parse(
        "server:\n  discovery:\n    enabled: true\n    service-name: office\n    subtypes: [responses, bad.one, _messages]\n    auth-required: false\n    advertise-management: true\n",
    )
    .unwrap();
    let d = DiscoveryConfig::from_document(&cfg.document);
    let spec = build_service_spec(&d, "", 8317, true, || "8f3b".into(), lan(&["192.0.2.2", "fe80::2"])).unwrap();
    assert_eq!(spec.instance_name, "office-8F3B");
    assert_eq!(spec.service_type, DEFAULT_SERVICE_TYPE);
    assert_eq!(spec.subtypes, ["_responses", "_messages"]);
    assert_eq!(spec.advertised_ips.len(), 2);
    for record in ["tls=1", "auth_required=false", "management=true", "instance_id=8f3b"] {
        assert!(
            spec.text_records.iter().any(|r| r == record),
            "{record}: {:?}",
            spec.text_records
        );
    }
    // A bound IP narrows the advertised addresses; loopback and names are refused.
    let one = build_service_spec(
        &d,
        "192.0.2.2",
        8317,
        false,
        || "8F3B".into(),
        lan(&["192.0.2.2", "fe80::2"]),
    )
    .unwrap();
    assert_eq!(one.advertised_ips, ["192.0.2.2".parse::<IpAddr>().unwrap()]);
    let err =
        |host: &str| build_service_spec(&d, host, 8317, false, || "8F3B".into(), lan(&["192.0.2.2"])).unwrap_err();
    assert_eq!(
        err("127.0.0.1"),
        "discovery: LAN advertising is unavailable for loopback bind host \"127.0.0.1\""
    );
    assert_eq!(
        err("box.lan"),
        "discovery: refusing LAN advertising for non-IP bind host \"box.lan\""
    );
    assert_eq!(err("192.0.2.9"), "discovery: no interface owns bind host \"192.0.2.9\"");
    // Legacy layout reads the same; defaults fill service type and subtypes.
    let legacy = cpa_core::config::Config::parse("discovery:\n  enabled: true\n  service-type: ''\n").unwrap();
    let d = DiscoveryConfig::from_document(&legacy.document);
    assert!(d.enabled && d.auth_required.is_none());
    assert_eq!((d.service_type.as_str(), d.subtypes.len()), (DEFAULT_SERVICE_TYPE, 5));
}

#[test]
fn instance_id_persists_and_reuses_a_valid_file() {
    let dir = std::env::temp_dir().join(format!("cpa-discovery-id-{}", std::process::id()));
    let fresh = dir.join("fresh");
    let id = instance_id(Some(&fresh));
    assert!(id.len() == 4 && id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    assert_eq!(std::fs::read_to_string(fresh.join("instance_id")).unwrap(), id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(fresh.join("instance_id"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let given = dir.join("given");
    std::fs::create_dir_all(&given).unwrap();
    std::fs::write(given.join("instance_id"), " ab12\n").unwrap();
    assert_eq!(instance_id(Some(&given)), "AB12");
    std::fs::write(given.join("instance_id"), "zzzz").unwrap();
    assert_eq!(instance_id(Some(&given)), "AB12", "cached per directory");
    let _ = std::fs::rename(
        &dir,
        std::env::temp_dir().join(format!("cpa-trash-{}", std::process::id())),
    );
}

fn gw(v: Value) -> DiscoveredService {
    let mut d = DiscoveredService {
        instance_name: s(&v["instance_name"]),
        service_type: s(&v["service_type"]),
        domain: s(&v["domain"]),
        host: s(&v["host"]),
        port: v["port"].as_u64().unwrap_or(0) as u16,
        ipv4: ips(&v["ipv4"]),
        ipv6: ips(&v["ipv6"]),
        protocols: list(&v["protocols"]),
        features: list(&v["features"]),
        product: s(&v["product"]),
        auth_required: v["auth_required"].as_bool().unwrap_or(false),
        auth_methods: list(&v["auth_methods"]),
        node_role: s(&v["node_role"]),
        version: s(&v["version"]),
        ..Default::default()
    };
    d.endpoints = serde_json::from_value(v["endpoints"].clone()).unwrap_or_default();
    d.raw_txt = serde_json::from_value(v["raw_txt"].clone()).unwrap_or_default();
    d
}

/// The four gateways Go's generator feeds its fake browser (cmd_fixture_test.go).
fn gateways() -> Vec<DiscoveredService> {
    vec![
        gw(serde_json::json!({
            "instance_name": "CPA-8F3B", "service_type": "_ai-gateway._tcp", "domain": "local.", "host": "box.local.",
            "port": 8317, "ipv4": ["192.168.1.5", "10.0.0.2"], "ipv6": ["fe80::1", "2001:db8::5"],
            "protocols": ["chat-completions", "responses"], "features": ["chat", "evil\u{1b}[31m"],
            "product": "cliproxyapi", "auth_required": true, "auth_methods": ["api_key"],
            "endpoints": {"openai": "/v1", "anthropic": "/v1", "gemini": "/v1beta"},
            "node_role": "standalone", "version": "1", "raw_txt": {"tls": "1", "product": "cliproxyapi", "z": "<&>"}
        })),
        gw(
            serde_json::json!({"instance_name": "bare\u{202e}", "port": 1234, "ipv6": ["fe80::2"], "host": "gw.local."}),
        ),
        gw(serde_json::json!({"instance_name": "open", "port": 80, "host": "bad host.local.", "auth_required": true})),
        gw(
            serde_json::json!({"instance_name": "mapped", "port": 81, "ipv4": ["127.0.0.1"], "ipv6": ["::ffff:192.0.2.1", "::1"]}),
        ),
    ]
}

#[tokio::test]
async fn discover_output_matches_go_byte_for_byte() {
    let f = fixture("cmd");
    let runs = f["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 9);
    for run in runs {
        let name = s(&run["name"]);
        let result: Result<Vec<DiscoveredService>, String> = match name.as_str() {
            "text-full" | "json-full" => Ok(gateways()),
            "text-browse-error" => Err("discovery: browse query failed: boom".into()),
            "json-browse-error" => Err("discovery: browse query failed: <boom>".into()),
            n if n.ends_with("factory-error") => Err("no qualified physical interfaces found for LAN discovery".into()),
            _ => Ok(Vec::new()),
        };
        let opts = Options {
            timeout: Duration::from_millis(run["timeout_ms"].as_i64().unwrap().max(0) as u64),
            json: run["json"].as_bool().unwrap(),
            service_type: s(&run["service_type"]),
            ..Default::default()
        };
        let want_type = match s(&run["service_type"]).trim() {
            "" => DEFAULT_SERVICE_TYPE.to_owned(),
            t => t.to_owned(),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::run(&opts, &mut out, &mut err, |st, _| async move {
            assert_eq!(st, want_type);
            result
        })
        .await;
        assert_eq!(code, run["code"].as_i64().unwrap() as i32, "{name}");
        assert_eq!(String::from_utf8(out).unwrap(), s(&run["stdout"]), "{name}");
        assert_eq!(String::from_utf8(err).unwrap(), s(&run["stderr"]), "{name}");
    }
}

#[test]
fn terminal_sanitizing_and_filters_match_go() {
    let f = fixture("cmd");
    for case in f["sanitize"].as_array().unwrap() {
        assert_eq!(cli::sanitize_terminal(&s(&case["in"])), s(&case["out"]), "{case}");
        assert_eq!(cli::sanitize_display_host(&s(&case["in"])), s(&case["host"]), "{case}");
    }
    for case in f["interface_lists"].as_array().unwrap() {
        assert_eq!(cli::parse_interface_list(&list(&case["in"])), list(&case["out"]));
    }
    let dir = std::env::temp_dir().join(format!("cpa-discover-filters-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    for case in f["config_filters"].as_array().unwrap() {
        std::fs::write(&path, s(&case["yaml"])).unwrap();
        let got = cli::load_scan_filters(path.to_str().unwrap());
        assert_eq!(got, (list(&case["include"]), list(&case["exclude"])), "{case}");
    }
    assert_eq!(
        cli::load_scan_filters(dir.join("missing.yaml").to_str().unwrap()),
        Default::default()
    );
    let _ = std::fs::rename(
        &dir,
        std::env::temp_dir().join(format!("cpa-trash-f-{}", std::process::id())),
    );
    let cli_filters = (vec!["docker0".to_owned()], vec![]);
    let cfg_filters = (vec!["en0".to_owned()], vec!["awdl0".to_owned()]);
    assert_eq!(
        cli::resolve_filters(cli_filters.clone(), cfg_filters.clone()),
        cli_filters
    );
    assert_eq!(
        cli::resolve_filters(Default::default(), cfg_filters.clone()),
        cfg_filters
    );
}

#[test]
fn go_durations() {
    for (ms, want) in [
        (3000, "3s"),
        (60_000, "1m0s"),
        (1500, "1.5s"),
        (59_000, "59s"),
        (250, "250ms"),
        (3_723_000, "1h2m3s"),
    ] {
        assert_eq!(cli::go_duration(Duration::from_millis(ms)), want);
    }
}
