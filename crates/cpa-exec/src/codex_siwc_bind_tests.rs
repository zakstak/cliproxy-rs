use super::*;
use crate::codex_testkit::{Mock, json};
use serde_json::Value;

fn golden() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/codex_siwc_bind_go.json")).unwrap()
}

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("cpa-siwc-bind-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("auth")).unwrap();
    std::fs::create_dir_all(root.join("out")).unwrap();
    std::fs::canonicalize(root).unwrap()
}

/// The Go recorder's file layout: `->target` is a symlink, `/` a directory.
fn write_files(base: &Path, files: &Value, expand: &dyn Fn(&str) -> String) {
    for (name, content) in files.as_object().into_iter().flatten() {
        let (path, content) = (base.join(name), content.as_str().unwrap());
        if let Some(target) = content.strip_prefix("->") {
            std::os::unix::fs::symlink(expand(target), &path).unwrap();
        } else if content == "/" {
            std::fs::create_dir_all(&path).unwrap();
        } else {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
    }
}

/// Go's message, with a wrapped cause's OS text cut (`cause`), against ours.
fn assert_error(case: &Value, got: &str, expand: &dyn Fn(&str) -> String) {
    let want = expand(
        case["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{case}: unexpected error {got}")),
    );
    if case["cause"] == Value::Bool(true) {
        assert!(
            got.starts_with(&format!("{want}: ")),
            "{}: {got} vs {want}",
            case["name"]
        );
    } else {
        assert_eq!(got, want, "{}", case["name"]);
    }
}

#[test]
fn siwc_bind_matches_go() {
    let go = golden();
    for case in go["resolve"].as_array().unwrap() {
        let root = temp_root();
        let (dir, out) = (root.join("auth"), root.join("out"));
        let expand = |s: &str| {
            s.replace("$DIR", &dir.to_string_lossy())
                .replace("$OUT", &out.to_string_lossy())
        };
        write_files(&out, &case["outside"], &expand);
        write_files(&dir, &case["files"], &expand);
        let auth_dir = expand(case["auth_dir"].as_str().unwrap());
        match resolve(Path::new(&auth_dir), &expand(case["target"].as_str().unwrap())) {
            Ok(path) => assert_eq!(
                path,
                PathBuf::from(expand(
                    case["result"].as_str().unwrap_or_else(|| panic!("{case}: {path:?}"))
                )),
                "{}",
                case["name"]
            ),
            Err(e) => assert_error(case, &e, &expand),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    for case in go["device"].as_array().unwrap() {
        let root = temp_root();
        let dir = root.join("auth");
        write_files(&dir, &case["files"], &|s| s.to_owned());
        match device_id(&dir.join("c.json")) {
            Ok(id) if case["device"] == "generated" => {
                assert!(
                    crate::codex_siwc::is_valid_uuid(&id) && id == id.go_lower(),
                    "{case}: {id}"
                );
                assert!(!case["files"]["c.json"].as_str().unwrap().go_lower().contains(&id));
            }
            Ok(id) => assert_eq!(id, case["device"].as_str().unwrap(), "{}", case["name"]),
            Err(e) => assert_error(case, &e, &|s| s.to_owned()),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    let tokens = Tokens {
        access_token: "siwc-at".into(),
        refresh_token: "siwc-rt".into(),
        id_token: "siwc-id".into(),
        client_id: "issued-client".into(),
        scope: "openid chatgpt.tokens.use.direct".into(),
        scopes: vec!["openid".into(), "chatgpt.tokens.use.direct".into()],
        expired: "2026-10-22T18:00:00Z".into(),
        // The v8.0.35 golden predates `expires_at` (Go v8.0.67 06fd5b8b).
        expires_at: String::new(),
        last_refresh: "2026-10-22T17:00:00Z".into(),
        device_id: "e61bbe28-07ef-466d-8e5d-a344f94ab305".into(),
    };
    for case in go["save"].as_array().unwrap() {
        let root = temp_root();
        let dir = root.join("auth");
        write_files(&dir, &case["files"], &|s| s.to_owned());
        let path = dir.join(case["target"].as_str().filter(|t| !t.is_empty()).unwrap_or("c.json"));
        match save(&path, &tokens) {
            Ok(()) => assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                case["result"].as_str().unwrap_or_else(|| panic!("{case}: saved")),
                "{}",
                case["name"]
            ),
            Err(e) => assert_error(case, &e, &|s| s.to_owned()),
        }
        // Go's lock file (`canonicalLockPath` + `.lock`) stays beside the credential.
        let mut listing: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        listing.sort();
        let want: Vec<String> = serde_json::from_value(case["listing"].clone()).unwrap();
        assert_eq!(listing, want, "{}", case["name"]);
        let _ = std::fs::remove_dir_all(root);
    }

    for case in go["callback"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let s = |k: &str| case[k].as_str().unwrap_or_default().to_owned();
        match parse_callback_input(input) {
            Ok(Some(c)) => assert_eq!(
                (c.code, c.state, c.client_id, c.error, c.description),
                (s("code"), s("state"), s("client_id"), s("error"), s("desc")),
                "{input}"
            ),
            Ok(None) => panic!("{input}: nothing parsed"),
            Err(message) => assert_eq!(message, s("err"), "{input}"),
        }
    }
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
