//! YAML binary and tagged-scalar config policy. The input corpus preserves historical
//! edge cases, but contains no expected output or errors from another implementation.
use cliproxy_internal::config::Config;
use serde::Deserialize;

#[derive(Deserialize)]
struct Input {
    name: String,
    yaml: String,
}

#[test]
fn binary_and_tagged_inputs_follow_config_boundary_policy() {
    let inputs: Vec<Input> = serde_json::from_str(include_str!("config/fixtures/yaml_binary_inputs.json")).unwrap();
    for Input { name, yaml } in inputs {
        // Explicit refusal policy: malformed binary, non-text bytes, ambiguous keys,
        // duplicate anchors, invalid merges and unsupported plugins must fail closed.
        let refused = matches!(
            name.as_str(),
            "custom-tagged port"
                | "redefined anchor around a binary secret"
                | "redefined anchor"
                | "alternating redefinitions over a binary scalar"
                | "locally tagged api-keys key"
                | "locally tagged port key"
                | "aliased port key"
                | "non-specific number secret"
                | "non-specific port"
                | "literal null tag on text"
                | "literal int tag"
                | "NUL in a binary tag"
                | "NUL in a tag directive"
                | "NUL in a tag after a block scalar holding a quote"
                | "NUL in a tag after a BOM"
                | "canonically tagged merge key"
                | "canonically tagged port key"
                | "single-quoted merge key"
                | "double-quoted merge key"
                | "escaped merge key"
                | "literal block merge key"
                | "quoted merge key in an anchored section"
                | "merge of an aliased sequence"
                | "merge of a list holding an aliased sequence"
                | "shadowed merge of an aliased sequence"
                | "shadowed scalar merge"
                | "binary plugin pin through a tagged merge source"
                | "merge key inside a collection key"
                | "collection key"
                | "non-specific document marker secret"
                | "non-specific boolean word secret"
                | "binary inside a mapping key"
                | "binary inside a mapping key under management"
                | "invalid base64"
                | "missing padding"
                | "space in base64"
                | "invalid base64 in an unknown setting"
                | "not UTF-8"
                | "binary port"
                | "binary boolean"
                | "binary plugin pin"
                | "aliased binary plugin pin"
                | "binary plugin pin through a merge"
                | "shadowed binary plugin pin"
                | "binary plugin setting"
        );
        let result = Config::parse(&yaml);
        if refused {
            assert!(result.is_err(), "{name}: unsafe or unsupported input accepted");
            continue;
        }
        let cfg = result.unwrap_or_else(|e| panic!("{name}: {e:#}"));
        // Independent expectations from the input's literal text and binary payload:
        // c2VjcmV0 encodes 'secret'; YR== encodes 'a'; custom tags retain text.
        let secret = match name.as_str() {
            "plain secret"
            | "binary v8 secret"
            | "binary legacy secret"
            | "quoted binary secret"
            | "block binary secret"
            | "verbatim binary secret"
            | "escaped binary secret"
            | "short-form binary secret"
            | "escaped primary binary secret"
            | "named handle binary secret"
            | "non-specific tagged secret"
            | "aliased custom-tagged secret"
            | "secret after a BOM"
            | "merge key"
            | "binary secret through a tagged merge source"
            | "aliased binary secret"
            | "binary secret through a merge"
            | "binary secret through an aliased section" => "secret",
            "redefined secondary handle secret" | "custom-tagged secret" => "c2VjcmV0",
            "custom-tagged number secret" | "literal str tag" => "0x1F",
            "non-specific quoted null secret" => "null",
            "NUL escape in an untagged value" => "secret%00",
            "non-specific multi-line secret" => "first\nsecond",
            "unused bits" => "a",
            _ => "",
        };
        assert_eq!(cfg.management.secret_key, secret, "{name}: management key");
        let (port, proxy, keys): (u16, &str, &[&str]) = match name.as_str() {
            "merge of a list of aliased mappings" => (1, "http://p.lan:80", &[]),
            "merge of an aliased mapping in a sequence"
            | "merge of a locally tagged aliased mapping"
            | "merge of a locally tagged mapping"
            | "merge of a list holding a locally tagged mapping"
            | "merge of a locally tagged sequence"
            | "nested locally tagged merge" => (1, "", &[]),
            "aliased custom-tagged secret" | "aliased binary secret" => (0, "secret", &[]),
            "binary proxy-url" => (0, "http://proxy.lan:80", &[]),
            "binary api key" => (0, "", &["plain", "key"]),
            _ => (0, "", &[]),
        };
        assert_eq!(cfg.port, port, "{name}: port");
        assert_eq!(cfg.api_keys, keys, "{name}: client keys");
        assert_eq!(
            cfg.document["requests"]["proxy-url"].as_str().unwrap_or_default(),
            proxy,
            "{name}: proxy URL"
        );
    }
}

#[test]
fn generated_binary_strings_decode_as_utf8_without_changing_other_fields() {
    use base64::Engine as _;
    for text in ["", "synthetic-secret", "é漢字", "0x1F", "true", "a\nb"] {
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        for binary in [
            format!("!!binary '{encoded}'"),
            format!("!<tag:yaml.org,2002:binary> '{encoded}'"),
        ] {
            let cfg = Config::parse(&format!(
                "server: {{port: 8317}}\nmanagement: {{secret-key: {binary}}}\n"
            ))
            .unwrap();
            assert_eq!(cfg.management.secret_key, text);
            assert_eq!(cfg.port, 8317);
        }
    }
}
