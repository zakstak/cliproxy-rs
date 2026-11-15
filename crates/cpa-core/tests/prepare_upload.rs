//! Upload preparation checks security and effective config preservation. Historical
//! YAML inputs remain useful stimuli; recorded output, error prose and YAML layout do not.
use cpa_core::config::{Config, ConfigDocument};
use serde::Deserialize;
use serde_yaml_ng::Value;

#[derive(Deserialize)]
struct Input {
    name: String,
    yaml: String,
}

// Refusals are classified by the Rust upload policy: malformed/removed settings,
// unresolvable tags, copies of plaintext, unsafe rewriting, and bcrypt byte bounds.
fn must_refuse(name: &str) -> bool {
    matches!(
        name,
        "unparsable"
            | "secret too long for bcrypt"
            | "mapping anchor"
            | "scalar anchor"
            | "merge key"
            | "shared anchor"
            | "anchored legacy beside plaintext v8"
            | "anchored legacy beside hashed v8"
            | "quoted hex secret beside an unquoted copy"
            | "tagged copy of the secret"
            | "tagged mapping hiding a copy"
            | "binary secret beside its base64 copy"
            | "binary secret beside its decoded copy"
            | "invalid binary secret"
            | "binary secret with a space"
            | "binary secret without padding"
            | "verbatim binary secret beside its decoded copy"
            | "escaped verbatim binary secret beside a copy"
            | "escaped shorthand binary secret beside a copy"
            | "named handle binary secret"
            | "named handle binary secret beside a copy"
            | "redefined secondary handle secret"
            | "redefined secondary handle secret beside a copy"
            | "malformed escape in the secret tag"
            | "binary anchor after a NEL-ended comment"
            | "binary anchor after a NEL-ended comment beside a copy"
            | "binary tag after a NEL-ended comment"
            | "binary tag after a NEL-ended comment beside a copy"
            | "line separator in a string"
            | "aliased legacy binary secret"
            | "aliased legacy binary secret beside a copy"
            | "aliased v8 binary secret"
            | "aliased v8 binary secret beside a copy"
            | "binary secret through an aliased section"
            | "binary secret through an aliased section beside a copy"
            | "binary secret through a merge"
            | "binary secret through a merge beside a copy"
            | "verbatim short binary secret beside a copy"
            | "escaped primary binary secret beside a copy"
            | "aliased plain secret beside an unrelated tag"
            | "tagged null copy"
            | "tagged tilde copy"
            | "tagged NULL copy"
            | "aliased hex secret"
            | "binary host beside a merged secret"
            | "hex key beside a merged secret"
            | "merge key beside a hex value"
            | "73 bytes"
            | "73 bytes multibyte"
            | "secret aliased into trusted-proxies"
            | "secret aliased into the relay public-ip"
            | "binary secret that is not UTF-8"
            | "relay port above uint16"
            | "negative relay port"
            | "null group weight"
            | "non-integer legacy weight"
            | "group weight above the limit"
            | "undeclared nested antigravity field"
            | "null legacy weight"
            | "legacy weight above the limit"
            | "compatible weight above the limit"
    )
}

fn key(document: &ConfigDocument) -> &str {
    document
        .get(&["management", "secret-key"])
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[test]
fn upload_inputs_preserve_effective_fields_and_never_save_plaintext() {
    let inputs: Vec<Input> = serde_json::from_str(include_str!("fixtures/upload_inputs.json")).unwrap();
    let dir = std::env::temp_dir().join(format!("cpa-upload-policy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    for Input { name, yaml } in inputs {
        std::fs::write(&path, &yaml).unwrap();
        let result = cpa_core::config::prepare_for_upload(&path);
        let saved = std::fs::read_to_string(&path).unwrap();
        if must_refuse(&name) {
            let error = result.expect_err(&format!("{name}: must refuse unsafe upload"));
            assert_eq!(saved, yaml, "{name}: refused upload changed disk");
            // Error messages must explain the failure without echoing a credential.
            if let Ok(before) = ConfigDocument::parse(&yaml) {
                let secret = key(&before);
                if secret.len() >= 8 && !secret.starts_with("$2") {
                    assert!(!format!("{error:#}").contains(secret), "{name}: secret in error");
                }
            }
            continue;
        }
        result.unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let mut before = ConfigDocument::parse(&yaml).unwrap();
        let mut after = ConfigDocument::parse(&saved).unwrap();
        let original = key(&before).to_owned();
        let prepared = key(&after).to_owned();
        if !original.is_empty() {
            if original.len() == 60 && original.starts_with("$2") {
                assert_eq!(prepared, original, "{name}: existing hash changed");
            } else {
                assert!(
                    bcrypt::verify(&original, &prepared).unwrap(),
                    "{name}: hash does not verify input secret"
                );
            }
        }
        // Compare every effective field, not a snapshot's incidental YAML spelling.
        // The management secret is the only permitted effective value change.
        before.delete(&["management", "secret-key"]);
        after.delete(&["management", "secret-key"]);
        // The canonical inverse spelling takes precedence over the legacy relay
        // allow-list spelling. Upload cleanup may drop that shadowed legacy leaf.
        for document in [&mut before, &mut after] {
            let relay = &["oauth", "providers", "codex", "live-media-relay"];
            if document
                .get(&[relay[0], relay[1], relay[2], relay[3], "disable-private-remote-ips"])
                .is_some()
            {
                document.delete(&[relay[0], relay[1], relay[2], relay[3], "allow-private-remote-ips"]);
            }
        }
        assert_eq!(after.value(), before.value(), "{name}: another setting changed");
        cpa_core::config::prepare_for_upload(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            saved,
            "{name}: preparation is not idempotent"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn ordinary_upload_preserves_comments_and_reloads() {
    let dir = std::env::temp_dir().join(format!("cpa-upload-comments-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let yaml = "# operator note\nserver: {port: 8317}\nmanagement: {secret-key: synthetic-secret}\n";
    std::fs::write(&path, yaml).unwrap();
    cpa_core::config::prepare_for_upload(&path).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.starts_with("# operator note\n"));
    assert!(!saved.contains("synthetic-secret"));
    let cfg = Config::parse(&saved).unwrap();
    assert_eq!(cfg.port, 8317);
    assert!(bcrypt::verify("synthetic-secret", &cfg.management.secret_key).unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}
