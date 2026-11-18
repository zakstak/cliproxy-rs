use super::*;
use crate::providers::codex_testkit::{Mock, json};
use serde_json::Value;

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("cpa-siwc-bind-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("auth")).unwrap();
    std::fs::create_dir_all(root.join("out")).unwrap();
    std::fs::canonicalize(root).unwrap()
}

const OUTER: &str =
    r#"{"type":"codex","access_token":"outer-access","refresh_token":"outer-refresh","note":"operator"}"#;

#[test]
fn bind_target_is_a_regular_oauth_file_inside_the_real_auth_directory() {
    let root = temp_root();
    let dir = root.join("auth");
    let path = dir.join("account.json");
    std::fs::write(&path, OUTER).unwrap();
    std::fs::write(root.join("out/outside.json"), OUTER).unwrap();
    std::fs::create_dir(dir.join("folder")).unwrap();
    std::os::unix::fs::symlink(&path, dir.join("inside.json")).unwrap();
    std::os::unix::fs::symlink(root.join("out/outside.json"), dir.join("outside.json")).unwrap();
    std::os::unix::fs::symlink(&dir, root.join("alias")).unwrap();
    for (auth, target) in [
        (dir.clone(), "account.json".to_owned()),
        (dir.clone(), path.to_string_lossy().into_owned()),
        (dir.clone(), "inside.json".to_owned()),
        (root.join("alias"), "account.json".to_owned()),
        (
            root.join("alias"),
            root.join("alias/account.json").to_string_lossy().into_owned(),
        ),
        (root.join("out/../auth"), "account.json".to_owned()),
        (root.join("alias/../auth"), "account.json".to_owned()),
    ] {
        assert_eq!(resolve(&auth, &target).unwrap(), path, "{auth:?} {target}");
    }
    for target in [
        "",
        "  ",
        ".",
        "..",
        "../out/outside.json",
        "folder/account.json",
        r"folder\account.json",
        "missing.json",
        "outside.json",
        "folder",
    ] {
        assert!(resolve(&dir, target).is_err(), "unsafe or absent target {target}");
    }
    for target in [root.join("out/outside.json"), dir.clone()] {
        assert!(resolve(&dir, &target.to_string_lossy()).is_err(), "{target:?}");
    }
    assert!(resolve(Path::new(""), "account.json").is_err());
    assert!(resolve(&root.join("missing"), "account.json").is_err());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        OUTER,
        "resolution never writes tokens"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn binding_requires_a_typed_codex_oauth_credential() {
    let root = temp_root();
    let dir = root.join("auth");
    let path = dir.join("account.json");
    for invalid in [
        "not JSON",
        "[]",
        "null",
        "{}",
        r#"{"type":"claude","access_token":"a","refresh_token":"r"}"#,
        r#"{"type":7,"access_token":"a","refresh_token":"r"}"#,
        r#"{"type":"codex","refresh_token":"r"}"#,
        r#"{"type":"codex","access_token":7,"refresh_token":"r"}"#,
        r#"{"type":"codex","access_token":"a","refresh_token":"  "}"#,
    ] {
        std::fs::write(&path, invalid).unwrap();
        assert!(resolve(&dir, "account.json").is_err(), "{invalid}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    }
    // Existing credential type spellings remain accepted during test migration.
    std::fs::write(&path, r#"{"type":" CoDeX ","access_token":"a","refresh_token":"r"}"#).unwrap();
    assert_eq!(resolve(&dir, "account.json").unwrap(), path);
    // Duplicated type fields are a saved-file parser policy, not wire evidence.
    std::fs::write(
        &path,
        r#"{"type":"claude","type":"codex","access_token":"a","refresh_token":"r"}"#,
    )
    .unwrap();
    assert_eq!(resolve(&dir, "account.json").unwrap(), path);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn binding_reuses_a_valid_device_or_generates_a_uuid_without_writing_the_file() {
    let root = temp_root();
    let path = root.join("auth/account.json");
    for partition in [
        serde_json::json!({"device_id":"AD674BB2-09CC-4CF0-AC43-970AE5557235"}),
        serde_json::json!({"device_id":"invalid"}),
        serde_json::json!({"device_id":7}),
        serde_json::json!({}),
        serde_json::json!("not-a-partition"),
        Value::Null,
    ] {
        let body = serde_json::json!({"siwc":partition}).to_string();
        std::fs::write(&path, &body).unwrap();
        let id = device_id(&path).unwrap();
        assert!(uuid::Uuid::parse_str(&id).is_ok());
        assert_eq!(id, id.to_ascii_lowercase());
        if partition["device_id"].as_str().is_some_and(|s| s.starts_with("AD674")) {
            assert_eq!(id, "ad674bb2-09cc-4cf0-ac43-970ae5557235");
        } else {
            assert_eq!(uuid::Uuid::parse_str(&id).unwrap().get_version_num(), 4);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
    }
    std::fs::write(&path, "{").unwrap();
    assert!(device_id(&path).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

fn binding_tokens() -> Tokens {
    Tokens {
        access_token: "bound-access".into(),
        refresh_token: "bound-refresh".into(),
        id_token: "bound-id".into(),
        client_id: "issued-client".into(),
        scope: "openid chatgpt.tokens.use.direct".into(),
        scopes: vec!["openid".into(), "chatgpt.tokens.use.direct".into()],
        expired: "2026-11-12T14:30:00Z".into(),
        expires_at: "2026-11-12T14:33:00Z".into(),
        last_refresh: "2026-11-12T13:33:00Z".into(),
        device_id: "ad674bb2-09cc-4cf0-ac43-970ae5557235".into(),
    }
}

#[test]
fn binding_atomically_replaces_siwc_and_preserves_outer_tokens_and_operator_fields() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_root();
    let dir = root.join("auth");
    let path = dir.join("account.json");
    let original = serde_json::json!({"type":"codex","access_token":"outer-access","refresh_token":"outer-refresh",
        "note":"<operator>&","siwc":{"access_token":"stale","stale":"discard"},"use_siwc":false});
    std::fs::write(&path, original.to_string()).unwrap();
    save(&path, &binding_tokens()).unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for key in ["type", "access_token", "refresh_token", "note"] {
        assert_eq!(saved[key], original[key], "{key}");
    }
    assert_eq!(saved["use_siwc"], true);
    assert_eq!(
        saved["siwc"],
        serde_json::json!({
            "access_token":"bound-access","refresh_token":"bound-refresh","id_token":"bound-id","client_id":"issued-client",
            "scope":"openid chatgpt.tokens.use.direct","scopes":["openid","chatgpt.tokens.use.direct"],
            "expired":"2026-11-12T14:30:00Z","expires_at":"2026-11-12T14:33:00Z","last_refresh":"2026-11-12T13:33:00Z",
            "device_id":"ad674bb2-09cc-4cf0-ac43-970ae5557235"
        })
    );
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            std::ffi::OsString::from("account.json"),
            std::ffi::OsString::from("account.json.lock")
        ],
        "no leftover publish temporary"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn binding_save_refuses_invalid_or_missing_files_without_destroying_original_bytes() {
    let root = temp_root();
    let path = root.join("auth/account.json");
    for invalid in ["{", "null", r#"{"type":"claude"}"#] {
        std::fs::write(&path, invalid).unwrap();
        assert!(save(&path, &binding_tokens()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    }
    std::fs::remove_file(&path).unwrap();
    assert!(save(&path, &binding_tokens()).is_err());
    assert!(!path.exists());
    let spaced = root.join("auth/trailing.json ");
    std::fs::write(&spaced, OUTER).unwrap();
    save(&spaced, &binding_tokens()).unwrap();
    assert!(spaced.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn binding_callbacks_decode_query_fragment_issued_client_and_oauth_denials() {
    for (input, code, state, client, error, description) in [
        (
            "http://127.0.0.1:1455/auth/callback?code=issued&state=nonce&client_id=%20client%20",
            "issued",
            "nonce",
            "client",
            "",
            "",
        ),
        (
            "?code=issued&state=nonce#client_id=fragment",
            "issued",
            "nonce",
            "fragment",
            "",
            "",
        ),
        (
            "?code=issued&state=nonce&client_id=query#client_id=fragment",
            "issued",
            "nonce",
            "query",
            "",
            "",
        ),
        (
            "http://localhost/cb#code=issued&state=nonce&client_id=fragment",
            "issued",
            "nonce",
            "fragment",
            "",
            "",
        ),
        (
            "?error=access_denied&error_description=denied&client_id=client",
            "",
            "",
            "client",
            "access_denied",
            "denied",
        ),
        (
            "?error_description=denied&client_id=client",
            "",
            "",
            "client",
            "denied",
            "",
        ),
    ] {
        let c = parse_callback_input(input).unwrap().unwrap();
        assert_eq!(
            (
                c.code.as_str(),
                c.state.as_str(),
                c.client_id.as_str(),
                c.error.as_str(),
                c.description.as_str()
            ),
            (code, state, client, error, description)
        );
    }
    let callback = parse_callback_input("?code=issued%23nonce&client_id=")
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            callback.code.as_str(),
            callback.state.as_str(),
            callback.client_id.as_str()
        ),
        ("issued", "nonce", "")
    );
}

/// The whole command against a loopback token endpoint: the stored device ID reaches
/// the authorization URL, the callback's issued client ID the exchange, and the file
/// keeps its outer tokens beside the new `siwc` partition. A callback with another
/// state saves nothing.
#[tokio::test]
async fn bind_saves_the_issued_client_tokens() {
    let mock = Mock::start().await;
    let token_path = "/api/accounts/oauth/token";
    let grant = r#"{"access_token":"siwc-at","refresh_token":"siwc-rt","id_token":"it","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#;
    mock.script(token_path, vec![json(200, grant)]);
    let root = temp_root();
    let dir = root.join("auth");
    let original = r#"{"type":"codex","access_token":"at","refresh_token":"rt","siwc":{"device_id":"E61BBE28-07EF-466D-8E5D-A344F94AB305"}}"#;
    std::fs::write(dir.join("c.json"), original).unwrap();
    let mut cfg = Config::parse("").unwrap();
    cfg.auth_dir = dir.clone();

    let run = |callback: &'static str| {
        let (cfg, url) = (cfg.clone(), format!("{}{token_path}", mock.url));
        async move {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            let interaction = Interaction {
                manual_delay: Duration::from_secs(3600),
                prompt: None,
                show_url: Box::new(move |url| {
                    let _ = tx.send(url.to_owned());
                }),
            };
            let oauth = CodexOAuth::new(wreq::Client::new()).with_siwc_token_url(&url);
            let task = tokio::spawn(async move { bind_with(&cfg, " c.json ", port, oauth, interaction).await });
            let auth_url = url::Url::parse(&rx.await.unwrap()).unwrap();
            let query = |k: &str| {
                auth_url
                    .query_pairs()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| v.into_owned())
                    .unwrap()
            };
            assert_eq!(
                query("ext_agent_host_id"),
                "urn:uuid:e61bbe28-07ef-466d-8e5d-a344f94ab305"
            );
            assert_eq!(query("redirect_uri"), format!("http://127.0.0.1:{port}/auth/callback"));
            let callback = callback.replace("{state}", &query("state"));
            let _ = wreq::Client::new()
                .get(format!("http://127.0.0.1:{port}/auth/callback?{callback}"))
                .send()
                .await
                .unwrap();
            (port, task.await.unwrap())
        }
    };

    let (_, mismatch) = run("code=the-code&state=other&client_id=oaiapp_issued").await;
    assert_eq!(
        mismatch,
        Err(BindError::Print(
            "Sign in with ChatGPT authentication failed: state mismatch".into()
        ))
    );
    assert!(mock.take().is_empty());
    assert_eq!(std::fs::read_to_string(dir.join("c.json")).unwrap(), original);

    let (port, bound) = run("code=the-code&state={state}&client_id=%20oaiapp_issued%20").await;
    let real = std::fs::canonicalize(dir.join("c.json")).unwrap();
    assert_eq!(bound, Ok(real.clone()));
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    let form: BTreeMap<String, String> = url::form_urlencoded::parse(&sent[0].body).into_owned().collect();
    assert_eq!(form["client_id"], "oaiapp_issued");
    assert_eq!(form["code"], "the-code");
    assert_eq!(form["redirect_uri"], format!("http://127.0.0.1:{port}/auth/callback"));
    let saved: Value = serde_json::from_slice(&std::fs::read(&real).unwrap()).unwrap();
    assert_eq!(
        (&saved["access_token"], &saved["refresh_token"]),
        (&serde_json::json!("at"), &serde_json::json!("rt"))
    );
    assert_eq!(saved["use_siwc"], serde_json::json!(true));
    assert_eq!(saved["siwc"]["access_token"], "siwc-at");
    assert_eq!(saved["siwc"]["client_id"], "oaiapp_issued");
    assert_eq!(saved["siwc"]["device_id"], "e61bbe28-07ef-466d-8e5d-a344f94ab305");
    let _ = std::fs::remove_dir_all(root);
}

/// #91 (Go v8.0.67 3891ebaa; OpenAI "Accounts and sessions": reauthorize with the
/// selected account's issued `client_id`, omit `agent_name_hint`, keep the host's
/// `ext_agent_host_id`): a file that already holds an issued SIWC client reauthorizes
/// with it. A callback naming another client, or a denied authorization, is a canceled
/// reauthorization: nothing is exchanged and the file keeps its bytes. A callback
/// without a client ID exchanges with the saved one, which the file keeps.
#[tokio::test]
async fn reauthorization_reuses_the_saved_issued_client() {
    let mock = Mock::start().await;
    let token_path = "/api/accounts/oauth/token";
    let grant = r#"{"access_token":"siwc-at2","refresh_token":"siwc-rt2","id_token":"it","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#;
    mock.script(token_path, vec![json(200, grant)]);
    let root = temp_root();
    let dir = root.join("auth");
    let original = r#"{"type":"codex","access_token":"at","refresh_token":"rt","siwc":{"access_token":"old-siwc-fake","refresh_token":"old-siwc-rt-fake","client_id":"oaiapp_saved","device_id":"e61bbe28-07ef-466d-8e5d-a344f94ab305"},"use_siwc":true}"#;
    std::fs::write(dir.join("c.json"), original).unwrap();
    let mut cfg = Config::parse("").unwrap();
    cfg.auth_dir = dir.clone();
    let run = |callback: &'static str| {
        let (cfg, url) = (cfg.clone(), format!("{}{token_path}", mock.url));
        async move {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            let interaction = Interaction {
                manual_delay: Duration::from_secs(3600),
                prompt: None,
                show_url: Box::new(move |url| {
                    let _ = tx.send(url.to_owned());
                }),
            };
            let oauth = CodexOAuth::new(wreq::Client::new()).with_siwc_token_url(&url);
            let task = tokio::spawn(async move { bind_with(&cfg, "c.json", port, oauth, interaction).await });
            let auth_url = url::Url::parse(&rx.await.unwrap()).unwrap();
            let query: BTreeMap<String, String> = auth_url.query_pairs().into_owned().collect();
            let callback = callback.replace("{state}", &query["state"]);
            let _ = wreq::Client::new()
                .get(format!("http://127.0.0.1:{port}/auth/callback?{callback}"))
                .send()
                .await
                .unwrap();
            (query, task.await.unwrap())
        }
    };
    let file = || std::fs::read_to_string(dir.join("c.json")).unwrap();

    let (query, other) = run("code=c&state={state}&client_id=oaiapp_other").await;
    assert_eq!(query.get("client_id").map(String::as_str), Some("oaiapp_saved"));
    assert!(!query.contains_key("agent_name_hint"), "{query:?}");
    assert_eq!(
        query["ext_agent_host_id"],
        "urn:uuid:e61bbe28-07ef-466d-8e5d-a344f94ab305"
    );
    assert_eq!(
        other,
        Err(BindError::Print(
            "Sign in with ChatGPT authentication failed: callback client ID does not match the saved registration"
                .into()
        ))
    );
    let (_, denied) = run("error=access_denied&state={state}").await;
    assert!(matches!(denied, Err(BindError::Print(_))), "{denied:?}");
    assert!(mock.take().is_empty(), "a canceled reauthorization exchanges nothing");
    assert_eq!(file(), original, "a canceled reauthorization keeps the file");

    let (_, bound) = run("code=c&state={state}").await;
    assert!(bound.is_ok(), "{bound:?}");
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    let form: BTreeMap<String, String> = url::form_urlencoded::parse(&sent[0].body).into_owned().collect();
    assert_eq!(form["client_id"], "oaiapp_saved");
    let saved: Value = serde_json::from_str(&file()).unwrap();
    assert_eq!(saved["siwc"]["client_id"], "oaiapp_saved");
    assert_eq!(saved["siwc"]["access_token"], "siwc-at2");
    let _ = std::fs::remove_dir_all(root);
}

/// #91 (Go 3891ebaa): a saved registration that is not a string, or that names the
/// dynamic registration client, is refused before any authorization starts.
#[tokio::test]
async fn reauthorization_refuses_an_unusable_saved_registration() {
    let root = temp_root();
    let dir = root.join("auth");
    let mut cfg = Config::parse("").unwrap();
    cfg.auth_dir = dir.clone();
    for (siwc, want) in [
        (r#"{"client_id":7}"#, "siwc-bind: invalid saved registration"),
        (
            r#"{"client_id":"dynamic_agent_client"}"#,
            "siwc-bind: failed to generate authorization URL: SIWC reauthorization requires the saved issued client ID",
        ),
    ] {
        let original = format!(r#"{{"type":"codex","access_token":"at","refresh_token":"rt","siwc":{siwc}}}"#);
        std::fs::write(dir.join("c.json"), &original).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let shown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let interaction = Interaction {
            manual_delay: Duration::from_secs(3600),
            prompt: None,
            show_url: Box::new({
                let shown = shown.clone();
                move |_| shown.store(true, std::sync::atomic::Ordering::SeqCst)
            }),
        };
        let oauth = CodexOAuth::new(wreq::Client::new()).with_siwc_token_url("http://127.0.0.1:9/token");
        let got = bind_with(&cfg, "c.json", port, oauth, interaction).await;
        assert_eq!(got, Err(BindError::Log(want.into())), "{siwc}");
        assert!(
            !shown.load(std::sync::atomic::Ordering::SeqCst),
            "{siwc}: no authorization URL"
        );
        assert_eq!(std::fs::read_to_string(dir.join("c.json")).unwrap(), original);
    }
    let _ = std::fs::remove_dir_all(root);
}

/// #91 (Go 88041f0c `canonicalLockPath`): the credential lock of a file reached through
/// a symlinked auth folder is the lock of its real path, so the server (which names the
/// file through the configured folder) and the CLI binding (which names its real path)
/// exclude each other.
#[test]
fn the_credential_lock_follows_symlinked_folders() {
    let root = temp_root();
    std::fs::write(root.join("auth/c.json"), "{}").unwrap();
    std::os::unix::fs::symlink(root.join("auth"), root.join("linked")).unwrap();
    let held = lock_file(&root.join("auth/c.json")).unwrap();
    let other = open_lock_file(&root.join("linked/c.json")).unwrap();
    assert!(!try_lock(&other).unwrap(), "the link's lock is the real file's lock");
    drop(held);
    assert!(try_lock(&other).unwrap());
    drop(other);
    // A file that does not exist yet locks through its folder's real path.
    let held = lock_file(&root.join("auth/new.json")).unwrap();
    assert!(!try_lock(&open_lock_file(&root.join("linked/new.json")).unwrap()).unwrap());
    drop(held);
    let _ = std::fs::remove_dir_all(root);
}
