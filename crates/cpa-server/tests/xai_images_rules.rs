//! xAI image routes through the real router, dispatch and executor against loopback
//! mocks (Go v8.0.67 826bb167 and 8a3e7622, issue #86): payload rules see what the
//! client left out, the answer follows the `response_format` the rules settled on, and
//! prefixed models route to the credential that registered the prefix, while a vendor
//! alias falls back to unprefixed credentials under `force-model-prefix`.

use std::sync::{Arc, Mutex};

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// `install_registry` sets the process-wide registry the image routes read (Go's
/// global registry): one test at a time, each using the runtime it installed last.
static REGISTRY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Requests one mock upstream received: path and JSON body.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

async fn read_request(conn: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = conn.read(&mut tmp).await.unwrap();
        assert!(n > 0, "upstream connection closed before the request was read");
        buf.extend_from_slice(&tmp[..n]);
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map_or(0, |v| v.trim().parse().unwrap());
        if buf.len() >= end + 4 + length {
            let path = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
            return (path, buf[end + 4..end + 4 + length].to_vec());
        }
    }
}

/// An xAI image upstream that answers in the format it was asked for, as xAI does.
async fn upstream() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen: Seen = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let (path, body) = read_request(&mut conn).await;
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let image = if body["response_format"] == "url" {
                    json!({"url": "https://imgen.x.ai/fake.png"})
                } else {
                    json!({"b64_json": "aW1n"})
                };
                log.lock().unwrap().push((path, body));
                let reply = json!({"created": 1, "data": [image]}).to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = conn.write_all(head.as_bytes()).await;
                let _ = conn.write_all(reply.as_bytes()).await;
                let _ = conn.shutdown().await;
            });
        }
    });
    (addr, seen)
}

struct Proxy {
    addr: String,
    _rt: Arc<cpa_server::Runtime>,
}

/// A proxy over xAI API-key credentials `(prefix, upstream)` with `extra` config.
async fn proxy(credentials: &[(&str, &str)], extra: &str) -> Proxy {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // A private auth-dir: without one, credential loading reads the real ~/.cli-proxy-api.
    let auth_dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("xai-images-rules-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&auth_dir).unwrap();
    let mut yaml = format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\n{extra}api-keys:\n  xai:\n",
        auth_dir.display()
    );
    for (i, (prefix, upstream)) in credentials.iter().enumerate() {
        yaml.push_str(&format!(
            "    - name: xai-{i}\n      base-url: http://{upstream}/v1\n      keys:\n        - api-key: sk-fake-xai-{i}\n"
        ));
        if !prefix.is_empty() {
            yaml.push_str(&format!("          prefix: {prefix}\n"));
        }
    }
    let config = Config::parse(&yaml).unwrap();
    let credentials = cpa_core::config::credentials::load(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    cpa_server::install_registry(&rt);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = cpa_server::router(rt.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Proxy { addr, _rt: rt }
}

/// One request; the status and the body text.
async fn send(addr: &str, path: &str, content_type: &str, body: &[u8]) -> (u16, String) {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer client-key\r\nContent-Type: {content_type}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    conn.write_all(head.as_bytes()).await.unwrap();
    conn.write_all(body).await.unwrap();
    let mut out = Vec::new();
    conn.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
    (status, body)
}

async fn generate(addr: &str, body: Value) -> (u16, String) {
    send(
        addr,
        "/v1/images/generations",
        "application/json",
        body.to_string().as_bytes(),
    )
    .await
}

fn json_body(status: u16, body: &str) -> Value {
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(body).unwrap_or_else(|_| panic!("{body}"))
}

const DEFAULTS: &str = "payload:\n  default:\n    - models: [{name: \"grok-imagine-*\"}]\n      params: {response_format: url, aspect_ratio: \"16:9\", resolution: 2k}\n";

/// A `default` rule fills the fields the client left out, though the route has its own
/// defaults for them (b64_json, 1:1, 1k); fields the client sent stay. The answer
/// follows the format sent upstream.
#[tokio::test]
async fn default_rules_fill_only_what_the_client_left_out() {
    let _registry = REGISTRY.lock().await;
    let (up, seen) = upstream().await;
    let p = proxy(&[("", &up)], DEFAULTS).await;
    let (status, body) = generate(&p.addr, json!({"model": "grok-imagine-image", "prompt": "draw"})).await;
    let out = json_body(status, &body);
    assert_eq!(out["data"][0]["url"], "https://imgen.x.ai/fake.png", "{out}");
    let sent = seen.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(
        (&sent["response_format"], &sent["aspect_ratio"], &sent["resolution"]),
        (&json!("url"), &json!("16:9"), &json!("2k")),
        "{sent}"
    );

    let explicit = json!({"model": "grok-imagine-image", "prompt": "draw", "response_format": "b64_json",
        "aspect_ratio": "1:1", "resolution": "1k"});
    let (status, body) = generate(&p.addr, explicit).await;
    let out = json_body(status, &body);
    assert_eq!(out["data"][0]["b64_json"], "aW1n", "{out}");
    let sent = seen.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(
        (&sent["response_format"], &sent["aspect_ratio"], &sent["resolution"]),
        (&json!("b64_json"), &json!("1:1"), &json!("1k")),
        "{sent}"
    );
}

/// An edit the client sent without `response_format` takes the `default` rule's, as
/// JSON and as a multipart form.
#[tokio::test]
async fn default_rules_fill_an_edits_response_format() {
    let _registry = REGISTRY.lock().await;
    let (up, seen) = upstream().await;
    let p = proxy(&[("", &up)], DEFAULTS).await;
    let edit =
        json!({"model": "grok-imagine-image", "prompt": "edit", "image": {"url": "https://example.invalid/a.png"}});
    let (status, body) = send(
        &p.addr,
        "/v1/images/edits",
        "application/json",
        edit.to_string().as_bytes(),
    )
    .await;
    let out = json_body(status, &body);
    assert_eq!(out["data"][0]["url"], "https://imgen.x.ai/fake.png", "{out}");
    let (path, sent) = seen.lock().unwrap().last().unwrap().clone();
    assert!(path.ends_with("/images/edits"), "{path}");
    assert_eq!(sent["response_format"], "url", "{sent}");

    let boundary = "fakeboundary";
    let mut form = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngrok-imagine-image\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n"
    )
    .into_bytes();
    form.extend_from_slice(b"\x89PNG\r\n");
    form.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let (status, body) = send(
        &p.addr,
        "/v1/images/edits",
        &format!("multipart/form-data; boundary={boundary}"),
        &form,
    )
    .await;
    let out = json_body(status, &body);
    assert_eq!(out["data"][0]["url"], "https://imgen.x.ai/fake.png", "{out}");
    assert_eq!(seen.lock().unwrap().last().unwrap().1["response_format"], "url");
}

/// An `override` rule's format decides what the client gets back, buffered and as the
/// emulated event stream: asked for `url`, overridden to `b64_json`, the image comes
/// back as `b64_json`, not as a data URL.
#[tokio::test]
async fn the_answer_follows_the_rule_settled_format() {
    let _registry = REGISTRY.lock().await;
    let (up, seen) = upstream().await;
    let rules = "payload:\n  override:\n    - models: [{name: \"grok-imagine-*\"}]\n      params: {response_format: b64_json}\n";
    let p = proxy(&[("", &up)], rules).await;
    let ask = json!({"model": "grok-imagine-image", "prompt": "draw", "response_format": "url"});
    let (status, body) = generate(&p.addr, ask.clone()).await;
    let out = json_body(status, &body);
    assert_eq!(seen.lock().unwrap().last().unwrap().1["response_format"], "b64_json");
    assert_eq!(out["data"][0]["b64_json"], "aW1n", "{out}");
    assert!(out["data"][0].get("url").is_none(), "{out}");

    let mut stream = ask;
    stream["stream"] = json!(true);
    let (status, body) = generate(&p.addr, stream).await;
    assert_eq!(status, 200, "{body}");
    let event = body
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap_or_else(|| panic!("{body}"));
    let event: Value = serde_json::from_str(event).unwrap();
    assert_eq!(event["b64_json"], "aW1n", "{event}");
    assert!(event.get("url").is_none(), "{event}");
}

fn served(seen: &Seen) -> usize {
    seen.lock().unwrap().len()
}

struct HomePick {
    credential: cpa_core::credential::Credential,
    models: Arc<Mutex<Vec<String>>>,
}

impl cpa_server::remote::RemoteDispatch for HomePick {
    fn available(&self) -> bool {
        true
    }

    fn dispatch(
        &self,
        request: cpa_server::remote::RemoteRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<cpa_server::remote::RemoteGrant, cpa_server::remote::RemoteError>>
    {
        self.models.lock().unwrap().push(request.model);
        let credential = self.credential.clone();
        Box::pin(async move {
            Ok(cpa_server::remote::RemoteGrant {
                credential,
                end: Box::new(|| None),
                cancel: None,
                request_retry: Some(0),
                user_api_key: "fake-home-principal".into(),
                private: Default::default(),
            })
        })
    }

    fn models(
        &self,
        _: Vec<(String, String)>,
        _: Vec<(String, String)>,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, cpa_server::remote::ModelsError>> {
        Box::pin(async { Ok(b"{}".to_vec()) })
    }
}

/// Home selects the credential; the same executor hook still determines the answer's
/// format from the finalized upstream request, for both buffered and streamed answers.
#[tokio::test]
async fn home_selected_credentials_use_the_rule_settled_format() {
    let _registry = REGISTRY.lock().await;
    let (up, seen) = upstream().await;
    let p = proxy(&[("", &up)], DEFAULTS).await;
    let auth_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("xai-images-home");
    let mut credential = cpa_core::credential::Credential::from_file(
        &auth_dir,
        &auth_dir.join("fake-xai.json"),
        json!({"type": "xai"}).as_object().unwrap().clone(),
    )
    .unwrap();
    credential
        .attributes
        .insert("api_key".into(), "sk-fake-home-xai".into());
    credential
        .attributes
        .insert("base_url".into(), format!("http://{up}/v1"));
    let models = Arc::new(Mutex::new(Vec::new()));
    p._rt.set_remote_dispatch(Some(Arc::new(HomePick {
        credential,
        models: models.clone(),
    })));

    for stream in [false, true] {
        let (status, body) = generate(
            &p.addr,
            json!({"model": "xai/grok-imagine-image", "prompt": "draw", "stream": stream}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(seen.lock().unwrap().last().unwrap().1["response_format"], "url");
        let out = if stream {
            let event = body.lines().find_map(|line| line.strip_prefix("data: ")).unwrap();
            serde_json::from_str::<Value>(event).unwrap()
        } else {
            json_body(status, &body)["data"][0].clone()
        };
        assert_eq!(out["url"], "https://imgen.x.ai/fake.png", "{out}");
        assert!(out.get("b64_json").is_none(), "{out}");
    }
    assert_eq!(*models.lock().unwrap(), vec!["xai/grok-imagine-image"; 2]);
}

const FORCED: &str = "routing:\n  force-model-prefix: true\n";

/// A prefix an xAI credential registered routes to that credential: `team/` reaches the
/// `team` credential, and `xai/` reaches a credential whose prefix is `xai`.
#[tokio::test]
async fn registered_prefixes_route_to_their_credential() {
    let _registry = REGISTRY.lock().await;
    for extra in ["", FORCED] {
        let (team, team_seen) = upstream().await;
        let (plain, plain_seen) = upstream().await;
        let p = proxy(&[("team", &team), ("", &plain)], extra).await;
        let (status, body) = generate(&p.addr, json!({"model": "team/grok-imagine-image", "prompt": "draw"})).await;
        assert_eq!(status, 200, "{extra:?}: {body}");
        assert_eq!((served(&team_seen), served(&plain_seen)), (1, 0), "{extra:?}");
        assert_eq!(team_seen.lock().unwrap()[0].1["model"], "grok-imagine-image");

        let (vendor, vendor_seen) = upstream().await;
        let (plain, plain_seen) = upstream().await;
        let p = proxy(&[("xai", &vendor), ("", &plain)], extra).await;
        let (status, body) = generate(&p.addr, json!({"model": "xai/grok-imagine-image", "prompt": "draw"})).await;
        assert_eq!(status, 200, "{extra:?}: {body}");
        assert_eq!((served(&vendor_seen), served(&plain_seen)), (1, 0), "{extra:?}");
    }
}

/// Under `force-model-prefix`, a vendor alias no credential registered falls back to
/// the unprefixed credentials (8a3e7622); any other unregistered prefix is refused.
#[tokio::test]
async fn vendor_aliases_fall_back_under_force_model_prefix() {
    let _registry = REGISTRY.lock().await;
    let (team, team_seen) = upstream().await;
    let (plain, plain_seen) = upstream().await;
    let p = proxy(&[("team", &team), ("", &plain)], FORCED).await;
    for model in [
        "xai/grok-imagine-image",
        "x-ai/grok-imagine-image-quality",
        "GROK/grok-imagine-image",
    ] {
        let (status, body) = generate(&p.addr, json!({"model": model, "prompt": "draw"})).await;
        assert_eq!(status, 200, "{model}: {body}");
    }
    assert_eq!((served(&team_seen), served(&plain_seen)), (0, 3));
    let (status, body) = generate(&p.addr, json!({"model": "other/grok-imagine-image", "prompt": "draw"})).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("is not supported"), "{body}");
    assert_eq!((served(&team_seen), served(&plain_seen)), (0, 3));
}
