//! Differential tests against `tests/fixtures/xai_auth_go.json`, produced by the real Go
//! login manager, FileTokenStore and XAIExecutor.Refresh (tests/reference/xai_auth).
//! The same scripted answers are replayed to the Rust port; requests, saved files,
//! refreshed metadata and error messages are compared. Expected values come only from Go.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;

const FIXTURE: &str = include_str!("../tests/fixtures/xai_auth_go.json");

/// Cases whose Go error is an encoding/json syntax message, which is not ported (see
/// `go_unmarshal`): only the text before it is compared.
/// Recordings that start from a file at the v8.0.35 name holding no identity, which
/// v8.0.67 (d65840fb) no longer claims (docs/DIFFERENCES-FROM-GO.md, "xAI credential
/// file names").
const UNCLAIMED: &[&str] = &[
    "subject_only_merges_existing_file",
    "existing_invalid_weight_blocks_save",
];

const SYNTAX_MESSAGE: &[&str] = &["discovery_not_json", "token_body_not_json"];

/// Go's error with the number it quotes in a type mismatch removed ("number 1.5 into"
/// becomes "number into"): Rust names only the kind, as response data never reaches
/// error text.
fn without_quoted_number(want: &str) -> String {
    const MARK: &str = "cannot unmarshal number ";
    match want
        .split_once(MARK)
        .and_then(|(head, rest)| Some((head, rest.split_once(" into ")?.1)))
    {
        Some((head, tail)) => format!("{head}cannot unmarshal number into {tail}"),
        None => want.to_owned(),
    }
}

#[derive(Clone)]
struct Reply {
    path: String,
    status: u16,
    body: String,
}

/// A raw HTTP/1.1 capture server: one request per connection, answered with the first
/// scripted reply for its path (Go generator `server`).
struct Mock {
    addr: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    async fn start(script: &Value) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let script: Vec<Reply> = script
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| Reply {
                path: r["path"].as_str().unwrap().to_owned(),
                status: r["status"].as_u64().unwrap() as u16,
                body: r["body"].as_str().unwrap().to_owned(),
            })
            .collect();
        let script = Arc::new(Mutex::new(script));
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = requests.clone();
        let host = addr.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (script, sink, host) = (script.clone(), sink.clone(), host.clone());
                tokio::spawn(async move {
                    let (read, mut write) = socket.into_split();
                    let mut reader = BufReader::new(read);
                    let mut raw = Vec::new();
                    let (mut length, mut path) = (0usize, String::new());
                    loop {
                        let mut line = Vec::new();
                        if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        raw.extend_from_slice(&line);
                        let text = String::from_utf8_lossy(&line).into_owned();
                        if path.is_empty() {
                            path = text.split_whitespace().nth(1).unwrap_or_default().to_owned();
                        }
                        if let Some(v) = text.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap_or(0);
                        }
                        if line == b"\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).await.unwrap();
                    raw.extend_from_slice(&body);
                    sink.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&raw).replace(&host, "UPSTREAM"));
                    let reply = {
                        let mut script = script.lock().unwrap();
                        match script.iter().position(|r| r.path == path) {
                            Some(i) => script.remove(i),
                            None => Reply {
                                path,
                                status: 599,
                                body: "unscripted".into(),
                            },
                        }
                    };
                    let reason = http::StatusCode::from_u16(reply.status)
                        .ok()
                        .and_then(|s| s.canonical_reason())
                        .unwrap_or("");
                    let out = format!(
                        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        reply.status,
                        reply.body.len(),
                        reply.body
                    );
                    let _ = write.write_all(out.as_bytes()).await;
                    let _ = write.shutdown().await;
                });
            }
        });
        Self { addr, requests }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn auth(&self) -> XaiAuth {
        XaiAuth::new(crate::proxy::default_client())
            .with_issuer_origin(&format!("http://{}", self.addr))
            .with_min_poll_interval(Duration::from_millis(1))
    }
}

/// Go's fixture replaces RFC 3339 UTC timestamps with `TIME`.
fn normalize_times(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let candidate = &b[i..(i + 20).min(b.len())];
        let shape = b"dddd-dd-ddTdd:dd:ddZ";
        if candidate.len() == 20
            && candidate
                .iter()
                .zip(shape)
                .all(|(c, p)| if *p == b'd' { c.is_ascii_digit() } else { c == p })
        {
            out.push_str("TIME");
            i += 20;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

#[test]
fn endpoint_validation_matches_go_net_url() {
    let f = fixture();
    let cases = f["validate"].as_array().unwrap();
    assert!(cases.len() >= 55, "fixture lost validation vectors");
    for case in cases {
        let raw = case[0].as_str().unwrap();
        let got = validate_endpoint(raw, "token_endpoint");
        let want = if case[1].as_bool().unwrap() {
            Ok(case[2].as_str().unwrap().to_owned())
        } else {
            Err(case[2].as_str().unwrap().to_owned())
        };
        assert_eq!(got, want, "ValidateOAuthEndpoint({raw:?})");
    }
}

/// The v8.0.35 recordings name files without the identity hash: those names are what
/// a login now recognizes as an earlier file of the account.
#[test]
fn legacy_file_names_match_go_v8_0_35() {
    for case in fixture()["file_names"].as_array().unwrap() {
        let (email, subject, want) = (
            case[0].as_str().unwrap(),
            case[1].as_str().unwrap(),
            case[2].as_str().unwrap(),
        );
        assert_eq!(
            legacy_credential_file_name(email, subject),
            want,
            "{email:?} {subject:?}"
        );
    }
}

/// Hashed names as Go v8.0.67 gives them (tests/reference/xai_filename): distinct
/// accounts whose sanitized names collide get distinct files, the same account keeps
/// one file whatever the email's case and whether the subject is known.
#[test]
fn credential_file_names_match_go_v8_0_67() {
    let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/xai_filename_go.json")).unwrap();
    let names = fixture["names"].as_array().unwrap();
    assert!(names.len() >= 20, "fixture lost cases");
    for case in names {
        let (email, subject) = (case["email"].as_str().unwrap(), case["subject"].as_str().unwrap());
        assert_eq!(
            credential_file_name(email, subject, 0),
            case["name"],
            "{email:?} {subject:?}"
        );
        assert_eq!(
            legacy_credential_file_name(email, subject),
            case["legacy"],
            "{email:?} {subject:?}"
        );
    }
    let name = |email: &str, subject: &str| credential_file_name(email, subject, 0);
    for (a, b) in [("a+b@x.ai", "a/b@x.ai"), ("a/b@x.ai", "a b@x.ai")] {
        assert_eq!(legacy_credential_file_name(a, ""), legacy_credential_file_name(b, ""));
        assert_ne!(name(a, ""), name(b, ""), "{a} and {b} share a file");
    }
    assert_ne!(name("", "acct/7"), name("", "acct 7"));
    assert_ne!(name("", "Sub-A"), name("", "sub-a"));
    assert_eq!(name("A.B+C@X.AI", ""), name("a.b+c@x.ai", "other-subject"));
}

#[tokio::test]
async fn login_matches_go_manager_and_file_store() {
    let f = fixture();
    let cases = f["login"].as_array().unwrap();
    assert!(cases.len() >= 22, "fixture lost login cases");
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let mock = Mock::start(&case["script"]).await;
        let dir = std::env::temp_dir().join(format!("xai-login-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        if let Some(existing) = case["existing"].as_str() {
            std::fs::write(dir.join(case["existing_name"].as_str().unwrap()), existing).unwrap();
        }
        let result = login_with(mock.auth(), &dir, true).await;
        if UNCLAIMED.contains(&name) {
            // v8.0.67: a file at the old name without a stored identity is ambiguous
            // (another account can sanitize to that name), so it is neither merged
            // nor removed; the login saves its own hashed file.
            let path = result.unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let existing = dir.join(case["existing_name"].as_str().unwrap());
            assert_eq!(
                std::fs::read_to_string(&existing).unwrap(),
                case["existing"].as_str().unwrap()
            );
            let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let sub = saved["sub"].as_str().unwrap_or_default();
            assert_eq!(
                path.file_name().unwrap().to_str().unwrap(),
                credential_file_name("", sub, 0)
            );
            assert_eq!(
                legacy_credential_file_name("", sub),
                case["existing_name"].as_str().unwrap()
            );
            assert_eq!(
                (saved["disabled"].clone(), saved.get("prefix")),
                (Value::Bool(false), None)
            );
            let _ = std::fs::remove_dir_all(&dir);
            continue;
        }
        let want_requests: Vec<String> = case["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(mock.requests(), want_requests, "{name}: requests");
        match result {
            Ok(path) => {
                assert!(case["error"].is_null(), "{name}: Go failed with {}", case["error"]);
                let file_name = path.file_name().unwrap().to_str().unwrap().to_owned();
                let want_name = case["file_name"].as_str().unwrap();
                let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let (email, sub) = (
                    saved["email"].as_str().unwrap_or_default(),
                    saved["sub"].as_str().unwrap_or_default(),
                );
                if want_name
                    .trim_start_matches("xai-")
                    .trim_end_matches(".json")
                    .bytes()
                    .all(|b| b.is_ascii_digit())
                {
                    // Go names subject-less credentials after the current time.
                    let stem = file_name.trim_start_matches("xai-").trim_end_matches(".json");
                    assert!(stem.bytes().all(|b| b.is_ascii_digit()), "{name}: {file_name}");
                } else {
                    // v8.0.67 hashes the identity into the name Go v8.0.35 recorded.
                    assert_eq!(file_name, credential_file_name(email, sub, 0), "{name}: file name");
                    assert_eq!(
                        legacy_credential_file_name(email, sub),
                        want_name,
                        "{name}: v8.0.35 name"
                    );
                }
                let saved = std::fs::read_to_string(&path).unwrap();
                assert_eq!(normalize_times(&saved), case["file"].as_str().unwrap(), "{name}: file");
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
                }
            }
            Err(error) => {
                let got = String::from_utf8_lossy(&error.body).into_owned();
                let want = case["error"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}: Rust failed with {got}"));
                if SYNTAX_MESSAGE.contains(&name) {
                    let prefix = want.split("parse response: ").next().unwrap();
                    assert!(got.starts_with(prefix), "{name}: {got} vs {want}");
                } else {
                    assert_eq!(got, without_quoted_number(want), "{name}: error");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[tokio::test]
async fn refresh_matches_go_executor() {
    let f = fixture();
    for case in f["refresh"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mock = Mock::start(&case["script"]).await;
        let mut metadata: Map<String, Value> = case["metadata"].as_object().unwrap().clone();
        for value in metadata.values_mut() {
            if let Value::String(s) = value {
                *s = s.replace("UPSTREAM", &mock.addr);
            }
        }
        let credential = Credential::from_file(
            Path::new("/fixture"),
            Path::new("/fixture/xai-test.json"),
            metadata.clone(),
        )
        .unwrap();
        let result = refresh(&mock.auth(), &credential).await;
        let want_requests: Vec<String> = case["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(mock.requests(), want_requests, "{name}: requests");
        match result {
            Ok(patch) => {
                assert!(case["error"].is_null(), "{name}: Go failed with {}", case["error"]);
                patch.apply(&mut metadata);
                let got: Map<String, Value> = metadata
                    .into_iter()
                    .map(|(k, v)| match v {
                        Value::String(s) => (k, Value::String(normalize_times(&s.replace(&mock.addr, "UPSTREAM")))),
                        other => (k, other),
                    })
                    .collect();
                assert_eq!(Value::Object(got), case["metadata_out"], "{name}: metadata");
            }
            Err(error) => {
                let got = String::from_utf8_lossy(&error.body).into_owned();
                let want = case["error"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}: Rust failed with {got}"));
                // Go appends the token endpoint's body to status errors; Rust withholds it.
                let want = match want.split_once(": {") {
                    Some((head, _)) if head.contains("failed with status") => head,
                    _ => want,
                };
                assert_eq!(got, without_quoted_number(want), "{name}: error");
            }
        }
    }
}

#[test]
fn number_mismatch_never_quotes_the_value() {
    let fields = [("expires_in", Field::Int)];
    for body in [
        &br#"{"expires_in":1.5}"#[..],
        br#"{"expires_in":99999999999999999999}"#,
        br#"{"expires_in":-1e3}"#,
    ] {
        let Err(error) = go_unmarshal(body, &fields, "T", "Wire") else {
            panic!("{body:?} decoded");
        };
        assert_eq!(
            error,
            "json: cannot unmarshal number into Go struct field Wire.expires_in of type int"
        );
    }
    assert_eq!(
        without_quoted_number("x: json: cannot unmarshal number 1.5 into Go struct field .e of type int"),
        "x: json: cannot unmarshal number into Go struct field .e of type int"
    );
}

#[test]
fn poll_interval_follows_go() {
    // PollForToken: the device interval, at least 5s; the test knob replaces a missing one.
    assert_eq!(initial_interval(0, None), Duration::from_secs(5));
    assert_eq!(initial_interval(3, None), Duration::from_secs(5));
    assert_eq!(initial_interval(9, None), Duration::from_secs(9));
    assert_eq!(
        initial_interval(-1, Some(Duration::from_millis(7))),
        Duration::from_millis(7)
    );
    assert_eq!(
        initial_interval(2, Some(Duration::from_millis(7))),
        Duration::from_secs(2)
    );
}

#[tokio::test]
async fn refresh_is_single_flighted_per_token() {
    let script = serde_json::json!([
        {"path": "/oauth2/token", "status": 200, "body": "{\"access_token\":\"a\"}"},
        {"path": "/oauth2/token", "status": 200, "body": "{\"access_token\":\"b\"}"}
    ]);
    let mock = Mock::start(&script).await;
    let auth = mock.auth();
    let endpoint = format!("http://{}/oauth2/token", mock.addr);
    let (a, b) = tokio::join!(
        auth.refresh("rt-shared", &endpoint),
        auth.refresh(" rt-shared ", &endpoint)
    );
    assert_eq!(a.unwrap().access_token, b.unwrap().access_token);
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn slow_down_adds_the_poll_step() {
    // exchangeDeviceCode: slow_down adds minPollInterval when set, else 5s.
    let script = serde_json::json!([
        {"path": "/t", "status": 400, "body": "{\"error\":\"slow_down\"}"},
        {"path": "/t", "status": 400, "body": "{\"error\":\"slow_down\"}"}
    ]);
    let mock = Mock::start(&script).await;
    let endpoint = format!("http://{}/t", mock.addr);
    let mut interval = Duration::from_millis(10);
    let knob = mock.auth();
    assert_eq!(knob.exchange(&endpoint, "d", &mut interval).await.unwrap(), None);
    assert_eq!(interval, Duration::from_millis(11));
    let plain = XaiAuth::new(crate::proxy::default_client());
    assert_eq!(plain.exchange(&endpoint, "d", &mut interval).await.unwrap(), None);
    assert_eq!(interval, Duration::from_millis(5011));
}

#[test]
fn metadata_strings_follow_go_fmt_sprint() {
    for case in fixture()["sprint"].as_array().unwrap() {
        let (raw, want) = (case[0].as_str().unwrap(), case[1].as_str().unwrap());
        let mut metadata = Map::new();
        metadata.insert("type".into(), Value::from("xai"));
        metadata.insert("k".into(), serde_json::from_str(raw).unwrap());
        let credential = Credential::from_file(Path::new("/f"), Path::new("/f/x.json"), metadata).unwrap();
        assert_eq!(metadata_string(&credential, "k"), want, "fmt.Sprint({raw})");
    }
}

#[test]
fn token_expiry_wraps_like_go_durations() {
    let now = DateTime::from_timestamp(1_000_000_000, 0).unwrap();
    for case in fixture()["expiry"].as_array().unwrap() {
        let mut decoded = Decoded::default();
        decoded.strs.insert("access_token", "a".into());
        decoded.ints.insert("expires_in", case[0].as_i64().unwrap());
        assert_eq!(
            token_data(&decoded, now).expire,
            case[1].as_str().unwrap(),
            "expires_in {}",
            case[0]
        );
    }
}

// --- collision-safe file names and migration (v8.0.67 d65840fb, issue #85) ---------------

/// A private auth directory under the system temp dir, removed on drop.
struct AuthDir(PathBuf);

impl AuthDir {
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("xai-names-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, name: &str, doc: Value) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
        path
    }

    fn read(&self, name: &str) -> Value {
        serde_json::from_slice(&std::fs::read(self.0.join(name)).unwrap()).unwrap()
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for AuthDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tokens(access: &str, email: &str, subject: &str) -> TokenData {
    TokenData {
        access_token: access.into(),
        refresh_token: format!("{access}-rt"),
        email: email.into(),
        subject: subject.into(),
        ..Default::default()
    }
}

fn record(access: &str, email: &str, subject: &str) -> LoginRecord {
    login_record(
        &tokens(access, email, subject),
        "2026-11-11T00:00:00Z",
        "https://auth.x.ai/oauth2/token",
        7,
    )
}

/// `save_login` as the CLI uses it: publish, then remove the earlier file.
fn save(dir: &AuthDir, record: LoginRecord) -> PathBuf {
    let (path, legacy) = save_login(&dir.0, record, crate::login::write_private).unwrap();
    if let Some(legacy) = legacy {
        std::fs::remove_file(legacy).unwrap();
    }
    path
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// Operator settings an earlier file can carry, and what the saved file must keep.
fn settings() -> Value {
    serde_json::json!({
        "proxy-url": "socks5://127.0.0.1:1080",
        "disabled": true,
        "priority": 7,
        "prefix": "team",
        "excluded_models": ["grok-x"],
        "note": "kept",
    })
}

fn assert_settings_kept(saved: &Value) {
    assert_eq!(saved["proxy_url"], "socks5://127.0.0.1:1080", "{saved}");
    assert_eq!(saved["disabled"], true, "{saved}");
    assert_eq!(saved["priority"], 7, "{saved}");
    assert_eq!(saved["prefix"], "team", "{saved}");
    assert_eq!(saved["excluded_models"], serde_json::json!(["grok-x"]), "{saved}");
    assert_eq!(saved["note"], "kept", "{saved}");
}

fn with(mut doc: Value, extra: Value) -> Value {
    for (k, v) in extra.as_object().unwrap() {
        doc[k] = v.clone();
    }
    doc
}

/// Two accounts whose emails sanitize alike: v8.0.35 wrote both to `xai-a-b@x.ai.json`,
/// the second login overwriting the first account's tokens.
#[test]
fn colliding_identities_keep_separate_files() {
    let dir = AuthDir::new("collide");
    let a = save(&dir, record("at-a", "a+b@x.ai", "sub-a"));
    let b = save(&dir, record("at-b", "a/b@x.ai", "sub-b"));
    assert_ne!(a, b);
    assert_eq!(dir.names().len(), 2, "{:?}", dir.names());
    assert_eq!(dir.read(&file_name(&a))["access_token"], "at-a");
    assert_eq!(dir.read(&file_name(&b))["access_token"], "at-b");
    // Subjects alike after sanitization stay apart too.
    let c = save(&dir, record("at-c", "", "acct/7"));
    let d = save(&dir, record("at-d", "", "acct 7"));
    assert_ne!(c, d);
    assert_eq!(dir.names().len(), 4, "{:?}", dir.names());
}

/// A repeat login refreshes the same file: new tokens, operator settings kept, and an
/// email in another case or a login without the subject is still the same file.
#[test]
fn repeat_login_refreshes_the_same_file() {
    let dir = AuthDir::new("repeat");
    let first = save(&dir, record("at-1", "Dev@X.ai", "sub-1"));
    let name = file_name(&first);
    let edited = with(dir.read(&name), settings());
    dir.write(&name, edited);
    for (access, email, subject) in [("at-2", "dev@x.ai", "sub-1"), ("at-3", "DEV@x.ai", "")] {
        let again = save(&dir, record(access, email, subject));
        assert_eq!(again, first);
        assert_eq!(dir.names(), vec![name.clone()]);
        let saved = dir.read(&name);
        assert_eq!(saved["access_token"], access);
        assert_settings_kept(&saved);
    }
}

/// A v8.0.35 file of the same account (email or subject name) moves to the hashed name
/// with its settings, and is removed only after the new file is saved.
#[test]
fn earlier_files_of_the_account_migrate_with_their_settings() {
    for (old_name, stored, email, subject) in [
        // v8.0.35 email name; the stored email differs only in case.
        (
            "xai-Dev@X.ai.json",
            serde_json::json!({"email": "Dev@X.ai", "sub": "sub-1"}),
            "dev@x.ai",
            "sub-1",
        ),
        // v8.0.35 subject name of a subject-only login, then a login with the email.
        (
            "xai-sub-1.json",
            serde_json::json!({"sub": "sub-1"}),
            "dev@x.ai",
            "sub-1",
        ),
        // Hashed subject-only name, then a login with the email.
        (
            &credential_file_name("", "sub-1", 0),
            serde_json::json!({"sub": "sub-1"}),
            "dev@x.ai",
            "sub-1",
        ),
    ] {
        let dir = AuthDir::new("migrate");
        let old = with(
            with(serde_json::json!({"type": "xai", "access_token": "old"}), stored),
            settings(),
        );
        dir.write(old_name, old);
        let path = save(&dir, record("at-new", email, subject));
        assert_eq!(file_name(&path), credential_file_name(email, subject, 0), "{old_name}");
        assert_eq!(dir.names(), vec![file_name(&path)], "{old_name}: earlier file removed");
        let saved = dir.read(&file_name(&path));
        assert_eq!(saved["access_token"], "at-new", "{old_name}");
        assert_settings_kept(&saved);
    }
}

/// Files a login must not claim: another account at the colliding v8.0.35 name, the
/// same email under another subject, an identity-less file, another provider's file,
/// and a file in a subdirectory.
#[test]
fn other_accounts_files_are_never_claimed() {
    let dir = AuthDir::new("foreign");
    let foreign = [
        (
            "xai-a-b@x.ai.json",
            serde_json::json!({"type": "xai", "email": "a b@x.ai", "access_token": "other"}),
        ),
        (
            "xai-sub-1.json",
            serde_json::json!({"type": "xai", "sub": "sub-1", "email": "z@x.ai", "access_token": "other"}),
        ),
        (
            "xai-a+b@x.ai.json",
            serde_json::json!({"type": "xai", "access_token": "other"}),
        ),
        (
            "xai-a-b@x.ai-x.json",
            serde_json::json!({"type": "codex", "email": "a+b@x.ai"}),
        ),
    ];
    for (name, doc) in &foreign {
        dir.write(name, doc.clone());
    }
    std::fs::create_dir_all(dir.0.join("nested")).unwrap();
    dir.write(
        "nested/xai-a-b@x.ai.json",
        serde_json::json!({"type": "xai", "email": "a+b@x.ai"}),
    );
    let mut before = dir.names();
    let path = save(&dir, record("at-a", "a+b@x.ai", "sub-1"));
    before.push(file_name(&path));
    before.sort();
    assert_eq!(dir.names(), before);
    for (name, doc) in &foreign {
        assert_eq!(&dir.read(name), doc, "{name} changed");
    }
    assert!(dir.0.join("nested/xai-a-b@x.ai.json").exists());
    let saved = dir.read(&file_name(&path));
    assert_eq!(
        (saved["disabled"].clone(), saved.get("note")),
        (Value::Bool(false), None)
    );
}

/// A save that fails leaves the earlier file as it was and writes nothing; a login
/// interrupted after the save but before the removal finishes on the next login.
#[test]
fn interrupted_saves_lose_nothing() {
    let dir = AuthDir::new("interrupted");
    let old = with(
        serde_json::json!({"type": "xai", "email": "dev@x.ai", "sub": "sub-1", "access_token": "old"}),
        settings(),
    );
    let legacy = dir.write("xai-dev@x.ai.json", old.clone());
    let failed = save_login(&dir.0, record("at-new", "dev@x.ai", "sub-1"), |_, _| {
        Err(std::io::Error::other("disk full"))
    });
    assert!(matches!(failed, Err(SaveError::Write(_))), "{failed:?}");
    assert_eq!(dir.names(), vec!["xai-dev@x.ai.json".to_owned()]);
    assert_eq!(dir.read("xai-dev@x.ai.json"), old);

    // Saved, not yet removed (the process stopped in between).
    let (path, pending) = save_login(
        &dir.0,
        record("at-new", "dev@x.ai", "sub-1"),
        crate::login::write_private,
    )
    .unwrap();
    assert_eq!(pending.as_deref(), Some(legacy.as_path()));
    assert_eq!(dir.names().len(), 2);
    let path_again = save(&dir, record("at-newer", "dev@x.ai", "sub-1"));
    assert_eq!(path_again, path);
    assert_eq!(dir.names(), vec![file_name(&path)]);
    let saved = dir.read(&file_name(&path));
    assert_eq!(saved["access_token"], "at-newer");
    assert_settings_kept(&saved);
}

/// Settings precedence: the new login's values, then the file at the new name, then
/// the earlier file; `disabled` comes from the file at the new name when it has one.
#[test]
fn settings_merge_in_go_order() {
    let dir = AuthDir::new("precedence");
    let name = credential_file_name("dev@x.ai", "sub-1", 0);
    dir.write(
        &name,
        serde_json::json!({"type": "xai", "email": "dev@x.ai", "disabled": false, "prefix": "new-file"}),
    );
    dir.write(
        "xai-dev@x.ai.json",
        with(
            serde_json::json!({"type": "xai", "email": "dev@x.ai", "access_token": "old"}),
            settings(),
        ),
    );
    let path = save(&dir, record("at-new", "dev@x.ai", "sub-1"));
    let saved = dir.read(&file_name(&path));
    assert_eq!(saved["prefix"], "new-file");
    assert_eq!(saved["disabled"], false);
    assert_eq!(saved["proxy_url"], "socks5://127.0.0.1:1080");
    assert_eq!(saved["access_token"], "at-new");
    assert_eq!(dir.names(), vec![name]);
}

/// A login that returns only the subject keeps refreshing the account's email-named
/// file, unless the subject-named file disables the credential.
#[test]
fn subject_only_login_keeps_the_email_named_file() {
    let dir = AuthDir::new("adopt");
    let email_name = credential_file_name("dev@x.ai", "sub-1", 0);
    dir.write(
        &email_name,
        with(
            serde_json::json!({"type": "xai", "email": "dev@x.ai", "sub": "sub-1", "access_token": "old"}),
            settings(),
        ),
    );
    let path = save(&dir, record("at-new", "", "sub-1"));
    assert_eq!(file_name(&path), email_name);
    assert_eq!(dir.names(), vec![email_name.clone()]);
    let saved = dir.read(&email_name);
    assert_eq!(
        (saved["access_token"].clone(), saved["email"].clone()),
        ("at-new".into(), "dev@x.ai".into())
    );
    assert_settings_kept(&saved);

    // The subject-named file disables the credential: it is saved there and the
    // email-named file is migrated into it.
    let subject_name = credential_file_name("", "sub-1", 0);
    dir.write(
        &subject_name,
        serde_json::json!({"type": "xai", "sub": "sub-1", "disabled": true}),
    );
    let path = save(&dir, record("at-newer", "", "sub-1"));
    assert_eq!(file_name(&path), subject_name);
    assert_eq!(dir.names(), vec![subject_name.clone()]);
    let saved = dir.read(&subject_name);
    assert_eq!(
        (saved["disabled"].clone(), saved["email"].clone()),
        (Value::Bool(true), "dev@x.ai".into())
    );
}

/// The CLI login migrates a v8.0.35 file through the device flow (the recorded
/// `success_after_pending` exchange against a loopback mock) and removes it.
#[tokio::test]
async fn cli_login_migrates_the_v8_0_35_file() {
    let f = fixture();
    let case = f["login"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "success_after_pending")
        .unwrap();
    let dir = AuthDir::new("cli");
    let old_name = case["file_name"].as_str().unwrap();
    dir.write(
        old_name,
        with(
            serde_json::json!({"type": "xai", "email": "dev.user+test@example.com", "access_token": "old"}),
            settings(),
        ),
    );
    let mock = Mock::start(&case["script"]).await;
    let path = login_with(mock.auth(), &dir.0, true).await.unwrap();
    assert_ne!(file_name(&path), old_name);
    assert_eq!(dir.names(), vec![file_name(&path)]);
    let saved = dir.read(&file_name(&path));
    assert_settings_kept(&saved);
    assert_ne!(saved["access_token"], "old");
}

// --- review fixes for #85: destination ownership, interrupted adoption, alias order ------

/// Every file in `dir`, with its bytes (symlinks as their target path).
fn snapshot(dir: &AuthDir) -> Vec<(String, Vec<u8>)> {
    dir.names()
        .into_iter()
        .map(|name| {
            let path = dir.0.join(&name);
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let bytes = if meta.file_type().is_symlink() {
                std::fs::read_link(&path).unwrap().into_os_string().into_encoded_bytes()
            } else if meta.is_dir() {
                b"<dir>".to_vec()
            } else {
                std::fs::read(&path).unwrap()
            };
            (name, bytes)
        })
        .collect()
}

/// Puts something at the destination path.
type Plant<'a> = Box<dyn Fn(&Path) + 'a>;

/// A file at the path a login would write that is not provably this account's is
/// refused before anything is merged, written or deleted. The first case needs no
/// hash collision: account B's subject reads like A's hashed name, and v8.0.35 saved B
/// as `xai-<subject>.json`, which is exactly A's new name.
#[test]
fn destinations_not_provably_this_accounts_are_refused() {
    let (email, subject) = ("dev@x.ai", "sub-A");
    let name = credential_file_name(email, subject, 0);
    let b_subject = name
        .strip_prefix("xai-")
        .unwrap()
        .strip_suffix(".json")
        .unwrap()
        .to_owned();
    assert_eq!(
        legacy_credential_file_name("", &b_subject),
        name,
        "B's v8.0.35 name is A's new name"
    );
    let foreign = |doc: Value| {
        with(
            doc,
            serde_json::json!({"access_token": "b-token", "proxy_url": "http://127.0.0.1:8087"}),
        )
    };
    let cases: Vec<(&str, Plant<'_>)> = vec![
        (
            "another account's v8.0.35 subject file",
            Box::new(|p: &Path| {
                std::fs::write(
                    p,
                    foreign(serde_json::json!({"type": "xai", "sub": b_subject})).to_string(),
                )
                .unwrap()
            }),
        ),
        (
            "same email under another subject",
            Box::new(|p: &Path| {
                std::fs::write(
                    p,
                    foreign(serde_json::json!({"type": "xai", "email": email, "sub": "sub-B"})).to_string(),
                )
                .unwrap()
            }),
        ),
        (
            "no stored identity",
            Box::new(|p: &Path| std::fs::write(p, foreign(serde_json::json!({"type": "xai"})).to_string()).unwrap()),
        ),
        (
            "another provider",
            Box::new(|p: &Path| {
                std::fs::write(
                    p,
                    foreign(serde_json::json!({"type": "codex", "email": email})).to_string(),
                )
                .unwrap()
            }),
        ),
        (
            "not JSON",
            Box::new(|p: &Path| std::fs::write(p, b"{\"type\":\"xai\",").unwrap()),
        ),
        ("empty", Box::new(|p: &Path| std::fs::write(p, b"").unwrap())),
        (
            "symlink to this account's own document",
            Box::new(|p: &Path| {
                let elsewhere = p.with_file_name("operator.txt");
                std::fs::write(
                    &elsewhere,
                    serde_json::json!({"type": "xai", "email": email, "sub": subject}).to_string(),
                )
                .unwrap();
                std::os::unix::fs::symlink(&elsewhere, p).unwrap();
            }),
        ),
        ("directory", Box::new(|p: &Path| std::fs::create_dir(p).unwrap())),
    ];
    for (label, plant) in cases {
        let dir = AuthDir::new("owned");
        plant(&dir.0.join(&name));
        // A matching earlier file of A stays too: nothing is deleted after a refusal.
        dir.write(
            "xai-dev@x.ai.json",
            with(
                serde_json::json!({"type": "xai", "email": email, "sub": subject, "access_token": "a-old"}),
                settings(),
            ),
        );
        let before = snapshot(&dir);
        let result = save_login(&dir.0, record("at-new", email, subject), crate::login::write_private);
        let error = result.as_ref().err().map(ToString::to_string).unwrap_or_default();
        assert!(result.is_err(), "{label}: saved over it: {result:?}");
        assert!(
            error.contains(&name) && error.contains("move it aside"),
            "{label}: {error}"
        );
        assert!(
            !error.contains("b-token") && !error.contains("8087"),
            "{label}: {error}"
        );
        assert_eq!(snapshot(&dir), before, "{label}: auth directory changed");
    }
}

/// An existing file that cannot be read is not mistaken for an absent one.
#[test]
fn an_unreadable_destination_is_not_overwritten() {
    use std::os::unix::fs::PermissionsExt;
    let dir = AuthDir::new("unreadable");
    let name = credential_file_name("dev@x.ai", "s", 0);
    let path = dir.write(
        &name,
        serde_json::json!({"type": "xai", "email": "dev@x.ai", "sub": "s", "note": "kept"}),
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&path).is_ok() {
        // Running with CAP_DAC_OVERRIDE: permissions cannot make the file unreadable.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        return;
    }
    let result = save_login(&dir.0, record("at-new", "dev@x.ai", "s"), crate::login::write_private);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let error = result.as_ref().err().map(ToString::to_string).unwrap_or_default();
    assert!(error.contains("cannot read"), "{result:?}");
    assert_eq!(dir.read(&name)["note"], "kept");
}

/// A migration from the subject-named file to the email-named one was interrupted
/// before its cleanup, and the next login returns only the subject: it refreshes the
/// email-named file and hands back the superseded subject-named file for deletion, so
/// one credential loads. Repeating that login changes nothing more, and another
/// account's file is never handed back.
#[test]
fn subject_only_retry_finishes_an_interrupted_migration() {
    let dir = AuthDir::new("adopt-retry");
    let subject_name = credential_file_name("", "s", 0);
    // An enabled credential: a subject-named file with `"disabled": true` keeps the
    // account on that file instead (subject_only_login_keeps_the_email_named_file).
    dir.write(
        &subject_name,
        serde_json::json!({"type": "xai", "sub": "s", "access_token": "old", "disabled": false,
            "proxy-url": "socks5://127.0.0.1:1080", "prefix": "team"}),
    );
    let other = dir.write(
        "xai-other@x.ai.json",
        serde_json::json!({"type": "xai", "email": "other@x.ai", "sub": "o"}),
    );
    let (email_path, pending) = save_login(&dir.0, record("at-1", "e@x.ai", "s"), crate::login::write_private).unwrap();
    assert_eq!(pending, Some(dir.0.join(&subject_name)));
    // The process stops here; the subject-named file is still on disk.
    let loaded = |dir: &AuthDir| {
        let cfg = cpa_core::config::Config {
            auth_dir: dir.0.clone(),
            ..Default::default()
        };
        let mut ids: Vec<String> = cpa_core::config::credentials::from_auth_dir(&cfg)
            .into_iter()
            .filter(|c| c.metadata.get("sub").and_then(Value::as_str) == Some("s"))
            .map(|c| c.id.clone())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(loaded(&dir).len(), 2);
    for access in ["at-2", "at-3"] {
        let path = save(&dir, record(access, "", "s"));
        assert_eq!(path, email_path, "{access}");
        assert_eq!(loaded(&dir).len(), 1, "{access}: {:?}", dir.names());
        let saved = dir.read(&file_name(&email_path));
        assert_eq!(
            (saved["access_token"].clone(), saved["email"].clone()),
            (access.into(), "e@x.ai".into())
        );
        assert_eq!(saved["proxy_url"], "socks5://127.0.0.1:1080", "{saved}");
        assert_eq!(
            (saved["prefix"].clone(), saved["disabled"].clone()),
            ("team".into(), false.into())
        );
        assert!(other.exists(), "{access}: another account's file removed");
    }
}

/// Each document's legacy keys are renamed before the merge, so its order (the new
/// login, the file at the new name, the earlier file) holds whatever spelling each file
/// uses. Within one file the canonical spelling wins, as Go's normalization does.
#[test]
fn aliases_keep_the_merge_order() {
    for (existing, earlier, want) in [
        (
            serde_json::json!({"proxy-url": "http://dest"}),
            serde_json::json!({"proxy_url": "http://earlier"}),
            "http://dest",
        ),
        (
            serde_json::json!({"proxy_url": "http://dest"}),
            serde_json::json!({"proxy-url": "http://earlier"}),
            "http://dest",
        ),
        (
            serde_json::json!({}),
            serde_json::json!({"proxy-url": "http://alias", "proxy_url": "http://canonical"}),
            "http://canonical",
        ),
        (
            serde_json::json!({"proxy_url": "http://canonical", "proxy-url": "http://alias"}),
            serde_json::json!({"proxy_url": "http://earlier"}),
            "http://canonical",
        ),
    ] {
        let dir = AuthDir::new("aliases");
        let identity = serde_json::json!({"type": "xai", "email": "dev@x.ai", "sub": "s"});
        let name = credential_file_name("dev@x.ai", "s", 0);
        dir.write(
            &name,
            with(
                with(identity.clone(), existing.clone()),
                serde_json::json!({"access_token": "stale", "excluded-models": ["dest"], "disabled": false}),
            ),
        );
        dir.write(
            "xai-dev@x.ai.json",
            with(
                with(identity, earlier.clone()),
                serde_json::json!({"excluded_models": ["earlier"], "disabled": true}),
            ),
        );
        let path = save(&dir, record("at-new", "dev@x.ai", "s"));
        let saved = dir.read(&file_name(&path));
        assert_eq!(saved["proxy_url"], want, "{existing} / {earlier}: {saved}");
        assert_eq!(saved.get("proxy-url"), None, "{saved}");
        assert_eq!(saved["excluded_models"], serde_json::json!(["dest"]), "{saved}");
        assert_eq!(saved.get("excluded-models"), None, "{saved}");
        assert_eq!(
            (saved["access_token"].clone(), saved["disabled"].clone()),
            ("at-new".into(), false.into())
        );
        assert_eq!(dir.names(), vec![name]);
    }
}

/// The CLI login refuses the same way, with the file named for the local operator and
/// no byte of the foreign file changed.
#[tokio::test]
async fn cli_login_refuses_another_accounts_file_at_its_name() {
    let f = fixture();
    let case = f["login"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "success_after_pending")
        .unwrap();
    let dir = AuthDir::new("cli-refuse");
    let name = credential_file_name("Dev.User+test@Example.com", "", 0);
    let foreign = serde_json::json!({"type": "codex", "email": "dev.user+test@example.com", "access_token": "other"});
    dir.write(&name, foreign.clone());
    let mock = Mock::start(&case["script"]).await;
    let error = login_with(mock.auth(), &dir.0, true).await.expect_err("refused");
    let message = String::from_utf8_lossy(&error.body).into_owned();
    assert!(
        message.contains(&name) && message.contains("move it aside"),
        "{message}"
    );
    assert!(!message.contains("other"), "{message}");
    assert_eq!(dir.read(&name), foreign);
    assert_eq!(dir.names(), vec![name]);
}
