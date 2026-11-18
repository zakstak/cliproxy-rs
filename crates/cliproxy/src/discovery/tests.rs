//! DNS-SD behavior and CLI output contracts with synthetic interfaces and browsers.
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use serde_json::Value;

use super::cli::{self, Options};
use super::iface::{self, Iface};
use super::*;

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
fn txt_metadata_preserves_supported_fields_with_dns_sd_size_limits() {
    // RFC 6763 section 6.1 limits each DNS-SD TXT string to 255 bytes.
    // The product additionally caps this advertisement at 400 bytes.
    let mut cases = vec![TxtOptions::default()];
    cases.extend([
        TxtOptions {
            instance_id: "AB12".into(),
            tls: true,
            advertise_management: true,
            ..Default::default()
        },
        TxtOptions {
            product: " custom ".into(),
            version: "2".into(),
            auth_required: false,
            ..Default::default()
        },
        TxtOptions {
            product: "nonascii-é\ncontrol\0".into(),
            ..Default::default()
        },
        TxtOptions {
            product: "p".repeat(256),
            ..Default::default()
        },
        TxtOptions {
            features: vec!["feature".repeat(50)],
            ..Default::default()
        },
        TxtOptions {
            api_path_openai: "/other".into(),
            api_path_anthropic: "/messages".into(),
            protocols: vec!["messages".into()],
            ..Default::default()
        },
    ]);
    for options in cases {
        let records = build_txt_records(&options);
        assert!(
            records
                .iter()
                .all(|record| record.len() <= 255 && record.bytes().all(|b| (b' '..=b'~').contains(&b)))
        );
        assert!(records.iter().map(|record| record.len() + 1).sum::<usize>() <= 400);
        assert!(
            !records.iter().any(|record| record.starts_with("api_gemini=")),
            "removed provider #49"
        );
        let metadata = parse_txt_records(&records);
        assert_eq!(metadata["version"], options.version);
        assert_eq!(metadata["tls"], if options.tls { "1" } else { "0" });
        assert_eq!(
            metadata["auth_required"],
            if options.auth_required { "true" } else { "false" }
        );
        assert_eq!(metadata["api_openai"], options.api_path_openai);
        assert_eq!(metadata["api_anthropic"], options.api_path_anthropic);
        if !options.instance_id.is_empty() {
            assert_eq!(metadata["instance_id"], options.instance_id);
        }
        if options.product.len() > 255 {
            assert!(!metadata.contains_key("product"));
        }
    }
}
#[test]
fn dns_sd_metadata_names_and_service_types_follow_explicit_boundary_rules() {
    let parsed = parse_txt_records(&strings(&[
        " VERSION =1",
        "tls=0",
        "TLS=1",
        "flag",
        "=ignored",
        "a=b=c",
    ]));
    assert_eq!(
        parsed,
        BTreeMap::from([
            ("version".into(), "1".into()),
            ("tls".into(), "1".into()),
            ("flag".into(), "".into()),
            ("a".into(), "b=c".into())
        ])
    );
    assert_eq!(format_instance_name("office", "ab12"), "office-AB12");
    assert_eq!(format_instance_name("", "AB12"), "CPA-AB12");
    assert_eq!(format_instance_name("office-AB12", "ab12"), "office-AB12");
    for input in ["x".repeat(100), "日本語".repeat(40), "\n\0 office \t".into()] {
        let name = sanitize_instance_name(&input);
        assert!(name.len() <= 63 && !name.chars().any(char::is_control));
        assert!(format_instance_name(&input, "AB12").len() <= 63);
    }
    for (input, expected) in [
        ("responses", "_responses"),
        ("_messages", "_messages"),
        ("bad.one", ""),
        ("", ""),
        ("_chat-completions", "_chat-completions"),
    ] {
        assert_eq!(sanitize_subtype(input), expected, "{input}");
    }
    for input in ["_ai-gateway._tcp", "_x._tcp", "_123456789012345._tcp"] {
        assert!(validate_service_type(input).is_ok(), "{input}");
    }
    for input in [
        "",
        "ai._tcp",
        "_x._udp",
        "_._tcp",
        "_-x._tcp",
        "_x-._tcp",
        "_bad.name._tcp",
        "_1234567890123456._tcp",
    ] {
        assert!(validate_service_type(input).is_err(), "{input}");
    }
    assert_eq!(
        parse_txt_list(" responses, messages, responses, , "),
        strings(&["responses", "messages"])
    );
    for (input, expected) in [(" /v1 ", "/v1"), ("v1", ""), ("", ""), ("https://bad/path", "")] {
        assert_eq!(sanitize_endpoint_path(input), expected, "{input}");
    }
}
#[test]
fn interface_name_policy_identifies_lan_tunnels_and_explicit_patterns() {
    for name in ["docker0", "veth123", "tailscale0", "wg0", "tun0", "tap1", "awdl0"] {
        assert!(iface::is_virtual_or_tunnel(name), "{name}");
    }
    for name in ["eth0", "en0", "wlan0", "wifi0", "bond0"] {
        assert!(iface::is_likely_physical_lan(name), "{name}");
        assert!(!iface::is_virtual_or_tunnel(name));
    }
    for (name, patterns, expected) in [
        ("eth0", vec!["ETH*"], true),
        ("wlan0", vec!["eth*"], false),
        ("en0", vec![" en0 "], true),
        ("en0", vec![""], false),
        ("eth0", vec!["eth1", "eth*"], true),
    ] {
        assert_eq!(
            iface::matches_any(name, &patterns.into_iter().map(str::to_owned).collect::<Vec<_>>()),
            expected
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
fn discovered_services_decode_metadata_and_merge_unique_addresses() {
    let entry = Entry {
        instance: "office._ai-gateway._tcp.local.".into(),
        service: "_ai-gateway._tcp".into(),
        domain: "local.".into(),
        host: "box.local.".into(),
        port: 8317,
        ipv4: vec!["192.0.2.1".parse().unwrap()],
        ipv6: vec!["2001:db8::1".parse().unwrap()],
        text: strings(&[
            "product=cliproxyapi",
            "protocols=responses,messages",
            "tls=1",
            "auth_required=true",
            "auth_methods=api_key",
            "api_openai=/v1",
        ]),
        nil_addr: false,
    };
    assert!(entry_within_limits(&entry));
    let mut actual = entry_to_discovered(&entry);
    assert_eq!(actual.host, "box.local.");
    assert_eq!(actual.port, 8317);
    assert_eq!(actual.product, "cliproxyapi");
    assert_eq!(actual.protocols, strings(&["responses", "messages"]));
    assert!(actual.auth_required);
    assert_eq!(actual.auth_methods, strings(&["api_key"]));
    assert_eq!(actual.endpoints["openai"], "/v1");
    assert_eq!(actual.ipv4, entry.ipv4);
    assert_eq!(actual.ipv6, entry.ipv6);
    let mut other = actual.clone();
    other.ipv4.push("192.0.2.2".parse().unwrap());
    other.host.clear();
    merge_discovered(&mut actual, &other);
    merge_discovered(&mut actual, &other);
    assert_eq!(
        actual.ipv4,
        vec!["192.0.2.1".parse::<IpAddr>().unwrap(), "192.0.2.2".parse().unwrap()]
    );
    assert_eq!(actual.host, "box.local.");
    let mut excessive = entry.clone();
    excessive.text = vec!["a=b".into(); 65];
    assert!(!entry_within_limits(&excessive));
    excessive = entry;
    excessive.ipv4 = vec!["192.0.2.1".parse().unwrap(); 33];
    assert_eq!(entry_to_discovered(&excessive).ipv4.len(), 32);
}
#[test]
fn advertising_refuses_invalid_service_types_and_zero_ports_before_network_access() {
    for service_type in ["bad", "_bad._udp", "_-bad._tcp"] {
        let cfg = DiscoveryConfig {
            enabled: true,
            service_type: service_type.into(),
            ..Default::default()
        };
        let error = build_service_spec(
            &cfg,
            "",
            8317,
            false,
            || "AB12".into(),
            |_, _| panic!("invalid spec must not query interfaces"),
        )
        .unwrap_err();
        assert!(error.contains("service"), "{error}");
    }
    let error = build_service_spec(
        &DiscoveryConfig::default(),
        "",
        0,
        false,
        || panic!("zero port needs no identity"),
        |_, _| panic!("zero port needs no interfaces"),
    )
    .unwrap_err();
    assert!(error.contains("port") && error.contains('0'), "{error}");
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
    let cfg = cliproxy_internal::config::Config::parse(
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
    let legacy = cliproxy_internal::config::Config::parse("discovery:\n  enabled: true\n  service-type: ''\n").unwrap();
    let d = DiscoveryConfig::from_document(&legacy.document);
    assert!(d.enabled && d.auth_required.is_none());
    assert_eq!((d.service_type.as_str(), d.subtypes.len()), (DEFAULT_SERVICE_TYPE, 3));
}

#[test]
fn instance_id_persists_and_reuses_a_valid_file() {
    let dir = std::env::temp_dir().join(format!("cpa-discovery-id-{}", std::process::id()));
    let fresh = dir.join("fresh");
    let id = instance_id(Some(&fresh));
    assert!(id.len() == 4 && id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    assert_eq!(std::fs::read_to_string(fresh.join("instance_id")).unwrap(), id);
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

/// Synthetic gateways cover IPv4, IPv6, escaping, auth, and unsafe terminal metadata.
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
async fn discovery_reports_semantic_json_safe_text_errors_and_effective_scan_options() {
    for json in [false, true] {
        for (timeout, expected_timeout) in [
            (Duration::ZERO, Duration::from_secs(3)),
            (Duration::from_millis(250), Duration::from_millis(250)),
            (Duration::from_secs(90), Duration::from_secs(60)),
        ] {
            let opts = Options {
                timeout,
                json,
                ..Default::default()
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = cli::run(&opts, &mut out, &mut err, |service_type, effective| async move {
                assert_eq!(service_type, DEFAULT_SERVICE_TYPE);
                assert_eq!(effective, expected_timeout);
                Ok(gateways())
            })
            .await;
            assert_eq!(code, 0);
            assert!(err.is_empty());
            let text = String::from_utf8(out).unwrap();
            if json {
                let list: Value = serde_json::from_str(&text).unwrap();
                let list = list.as_array().unwrap();
                assert_eq!(list.len(), 4);
                assert_eq!(list[0]["instance_name"], "CPA-8F3B");
                assert_eq!(list[0]["port"], 8317);
                assert_eq!(
                    list[0]["protocols"],
                    serde_json::json!(["chat-completions", "responses"])
                );
                assert_eq!(list[0]["raw_txt"]["z"], "<&>");
            } else {
                for expected in ["CPA-8F3B", "8317", "OpenAI", "responses", "api_key"] {
                    assert!(text.contains(expected), "{expected}: {text}");
                }
                assert!(
                    !text.contains('\u{1b}') && !text.contains('\u{202e}'),
                    "unsafe terminal control"
                );
            }
        }
        for result in [
            Ok(Vec::new()),
            Err("browse failed: <boom>".to_owned()),
            Err("no qualified interfaces".to_owned()),
        ] {
            let opts = Options {
                json,
                service_type: "_custom._tcp".into(),
                ..Default::default()
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let failure = result.as_ref().err().cloned();
            let code = cli::run(&opts, &mut out, &mut err, |service_type, _| async move {
                assert_eq!(service_type, "_custom._tcp");
                result
            })
            .await;
            assert_eq!(code, if failure.is_some() { 1 } else { 0 });
            if json {
                assert!(err.is_empty());
                let output: Value = serde_json::from_slice(&out).unwrap();
                if let Some(error) = failure {
                    assert_eq!(output, serde_json::json!({"error":error,"gateways":[]}));
                } else {
                    assert_eq!(output, serde_json::json!([]));
                }
            } else if let Some(error) = failure {
                assert!(String::from_utf8(err).unwrap().contains(&error));
            } else {
                assert!(String::from_utf8(out).unwrap().contains("No AI gateways"));
            }
        }
    }
}
#[test]
fn discovery_sanitizes_terminal_controls_and_respects_explicit_interface_filters() {
    for unsafe_text in ["\u{1b}[31mred", "a\u{202e}b", "a\u{200b}b", "a\0b", "line\nfeed"] {
        let clean = cli::sanitize_terminal(unsafe_text);
        assert!(
            !clean
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{202e}' | '\u{200b}')),
            "{clean:?}"
        );
    }
    assert_eq!(cli::sanitize_terminal("a\tb"), "a b");
    assert_eq!(
        cli::parse_interface_list(&[" eth0,en0 ", "eth0", "", "wl*"]),
        strings(&["eth0", "en0", "wl*"])
    );
    let dir = std::env::temp_dir().join(format!("cpa-discovery-contract-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    for yaml in [
        "discovery:\n interfaces:\n  include: [eth0, ' wl* ']\n  exclude: [docker0]\n",
        "filters: &filters\n include: [eth0, ' wl* ']\n exclude: [docker0]\ndiscovery:\n interfaces:\n  <<: *filters\n",
    ] {
        std::fs::write(&path, yaml).unwrap();
        assert_eq!(
            cli::load_scan_filters(path.to_str().unwrap()).unwrap(),
            (strings(&["eth0", "wl*"]), strings(&["docker0"]))
        );
    }
    for yaml in [
        "discovery:\n interfaces:\n  include: !!binary '!!!'\n",
        "a: &x 1\nb: &x 2\ndiscovery:\n interfaces:\n  exclude: [eth0]\n",
    ] {
        std::fs::write(&path, yaml).unwrap();
        assert!(
            cli::load_scan_filters(path.to_str().unwrap()).is_err(),
            "invalid config must not broaden discovery"
        );
    }
    assert_eq!(
        cli::load_scan_filters(dir.join("missing.yaml").to_str().unwrap()),
        Ok(Default::default())
    );
    let explicit = (strings(&["docker0"]), vec![]);
    let configured = (strings(&["en0"]), strings(&["awdl0"]));
    assert_eq!(cli::resolve_filters(explicit.clone(), configured.clone()), explicit);
    assert_eq!(cli::resolve_filters(Default::default(), configured.clone()), configured);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn scan_duration_display_is_readable_at_supported_timeout_boundaries() {
    for duration in [
        Duration::ZERO,
        Duration::from_millis(250),
        Duration::from_millis(1500),
        Duration::from_secs(60),
    ] {
        let shown = cli::go_duration(duration);
        assert!(!shown.is_empty() && shown.ends_with('s'), "{shown}");
        assert!(shown.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'), "{shown}");
    }
}
