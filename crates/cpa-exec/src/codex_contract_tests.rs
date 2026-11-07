//! Integrity of the Codex contract fixture set. These tests check the fixtures and the
//! reader only; no proxy behaviour is asserted here.

use std::collections::BTreeSet;
use std::path::Path;

use super::*;

fn files_under(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(root, &path, out);
        } else {
            out.insert(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
        }
    }
}

#[test]
fn manifest_and_directory_list_the_same_files() {
    let root = dir();
    let mut on_disk = BTreeSet::new();
    files_under(&root, &root, &mut on_disk);
    on_disk.remove("manifest.json");
    on_disk.remove("README.md");

    let manifest = manifest();
    let listed: BTreeSet<String> = manifest.fixtures.iter().map(|f| f.file.clone()).collect();
    assert_eq!(listed.len(), manifest.fixtures.len(), "a file is listed twice");
    assert_eq!(on_disk, listed);

    let names: BTreeSet<&str> = manifest.fixtures.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names.len(), manifest.fixtures.len(), "a name is used twice");
}

#[test]
fn every_fixture_records_its_provenance() {
    let manifest = manifest();
    assert_eq!(manifest.pin.commit, FIRST_PARTY_COMMIT);
    assert_eq!(manifest.pin.repository, "https://github.com/openai/codex");
    for f in &manifest.fixtures {
        assert!(!f.endpoints.is_empty(), "{}: no endpoint profile", f.name);
        assert!(!f.contract.trim().is_empty(), "{}: no contract", f.name);
        assert!(!f.sources.is_empty(), "{}: no source", f.name);
        match f.evidence {
            Evidence::FirstPartySource | Evidence::FirstPartyTest => {
                let first_party = f
                    .sources
                    .iter()
                    .filter(|s| s.starts_with("codex-rs/"))
                    .collect::<Vec<_>>();
                assert!(!first_party.is_empty(), "{}: no first-party path", f.name);
                for s in first_party {
                    let (path, _) = s.split_once(' ').unwrap_or((s, ""));
                    assert!(
                        path.rsplit_once(':')
                            .is_some_and(|(_, lines)| lines.starts_with(|c: char| c.is_ascii_digit())),
                        "{}: {s:?} has no line range",
                        f.name
                    );
                }
            }
            Evidence::ProviderDocs => assert!(
                f.sources
                    .iter()
                    .any(|s| s.starts_with("https://developers.openai.com/")),
                "{}: no documentation URL",
                f.name
            ),
            Evidence::ContractProbe => assert!(
                f.notes.contains("Not first-party"),
                "{}: a probe must say it is not first-party evidence",
                f.name
            ),
        }
    }
}

#[test]
fn every_frame_is_json() {
    for f in manifest().fixtures {
        let frames = f.frames();
        assert!(!frames.is_empty(), "{}: no frames", f.name);
        for frame in frames {
            serde_json::from_slice::<serde::de::IgnoredAny>(&frame)
                .unwrap_or_else(|e| panic!("{}: {e}: {}", f.name, String::from_utf8_lossy(&frame)));
        }
    }
}

/// Fixtures hold placeholders only: no bearer tokens, API keys, JWTs or token fields.
#[test]
fn fixtures_hold_no_credentials() {
    const FORBIDDEN: &[&str] = &[
        "Bearer ",
        "sk-",
        "eyJ",
        "access_token",
        "refresh_token",
        "id_token",
        "api_key",
        "chatgpt-account-id",
    ];
    for f in manifest().fixtures {
        let text = String::from_utf8(f.bytes().to_vec()).unwrap_or_else(|_| panic!("{}: not UTF-8", f.name));
        for needle in FORBIDDEN {
            assert!(!text.contains(needle), "{}: contains {needle:?}", f.name);
        }
    }
}

#[test]
fn frames_are_exact_slices() {
    let sse = fixture("sse-completed");
    let bytes = sse.bytes();
    let frames = sse.frames();
    assert_eq!(frames.len(), 5);
    assert!(frames[0].starts_with(br#"{"type":"response.created""#));
    assert!(frames[4].starts_with(br#"{"type":"response.completed""#));
    for frame in &frames {
        let at = bytes.windows(frame.len()).position(|w| w == &frame[..]).unwrap();
        assert_eq!(&bytes[at - 6..at], b"data: ");
    }

    let jsonl = fixture("ws-errors-first-party");
    assert_eq!(jsonl.frames().len(), 2);

    let json = fixture("probe-request-unknown-fields");
    assert_eq!(json.frames(), vec![json.bytes()]);
    // The probe keeps raw number spellings a serializer would rewrite.
    for spelling in ["1.50", "1e3", "-0.0", "12345678901234567890", r"caf\u00e9"] {
        assert!(
            json.bytes().windows(spelling.len()).any(|w| w == spelling.as_bytes()),
            "{spelling}"
        );
    }
}
