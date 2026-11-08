//! Codex request shaping: body rules and upstream headers, applied in Go's order so
//! appended keys land where Go puts them (codex_executor_execute.go, codex_executor_stream.go,
//! codex_executor_request.go, codex_websockets_request.go, helps/codex_input_ids.go,
//! helps/codex_tool_schema.go, openai_responses_signature.go).

use cpa_core::gostr::GoStr;
use std::time::Duration;

use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, FailureScope};
use cpa_core::format::Format;
use gjson::Kind;
use http::{HeaderMap, HeaderName, HeaderValue};

use crate::codex_identity::{self as identity, Origin};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub(crate) const WS_BETA: &str = "responses_websockets=2026-02-06";
const ROUTING_HINT: &str = "x-codex-routing-hint";
const LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

/// Codex settings read from the config snapshot (v8 paths; legacy keys are already moved).
#[derive(Debug, Clone, Default)]
pub(crate) struct Settings {
    /// `upstream.codex.disable-codex-cloaking`. Go's cloaking overwrote the client
    /// identity; that part is gone (#26, [`crate::codex_identity`]). What remains is its
    /// other effect: a native WebSocket handshake carries the caller's own session
    /// headers instead of the proxy's prompt-cache session.
    pub native_session_headers: bool,
    pub bootstrap_buffering: bool,
    pub bootstrap_timeout: Option<Duration>,
    pub model_level_cooling: bool,
    pub default_beta_features: String,
    /// `multimedia.disable-image-generation`: a Codex request carrying the tool it
    /// disables is refused, never stripped ([`Settings::refuse_image_tool`]).
    pub image_generation: cpa_common::payload::ImageGeneration,
    /// The `models` of the `codex-api-key` entry the credential resolves to.
    pub key_models: Vec<serde_json::Value>,
    /// The credential, when Home dispatched it (Go `cfg.Home.Enabled`): its
    /// `credential_options` decide is-compat when nothing else does.
    pub home: Option<cpa_core::credential::Credential>,
    /// `response-steering`: full-duplex WebSocket turns (codex_websockets_duplex.go).
    pub response_steering: bool,
    /// `multimedia.gpt-image-2-base-model`, trimmed: the Images API's tool-call model.
    pub image_base_model: String,
}

/// `upstream.codex.response-steering`; the older spellings are already moved there.
pub(crate) fn response_steering(cfg: &Config) -> bool {
    setting(cfg, &["upstream", "codex", "response-steering"])
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false)
}

/// `oauth.providers.codex.chatgpt-keep-alive`, a cliproxy-rs addition: off unless set to
/// true (YAML 1.1 spellings such as `yes` are normalised by the config schema).
pub(crate) fn chatgpt_keep_alive(cfg: &Config) -> bool {
    setting(cfg, &["oauth", "providers", "codex", "chatgpt-keep-alive"])
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false)
}

fn setting<'a>(cfg: &'a Config, path: &[&str]) -> Option<&'a serde_yaml_ng::Value> {
    path.iter().try_fold(&cfg.document, |v, k| v.get(*k))
}

impl Settings {
    /// The settings `view`'s credential sees: API keys get Go's `ForAPIKey` view, where
    /// the OAuth-only `oauth.providers.codex.*` settings (the header defaults) do not
    /// apply.
    pub fn scoped(cfg: &Config, view: &View<'_>) -> Self {
        let mut settings = if view.api_key {
            Self::from(&cfg.for_api_key())
        } else {
            Self::from(cfg)
        };
        // ponytail: Go's executor-side resolveCodexKeyConfig skips the prefix/proxy
        // tie-break of the shared resolver; they differ only for duplicate key+URL entries.
        settings.key_models = cpa_core::config::credentials::resolve_api_key_entry(cfg, "codex", view.credential)
            .map(|entry| entry.models)
            .unwrap_or_default();
        if view
            .credential
            .attributes
            .contains_key(cpa_core::config::credentials::HOME_PROVIDER)
        {
            settings.home = Some(view.credential.clone());
        }
        settings
    }

    /// Whether the request's model is marked `is-compat`, which no Codex route honours
    /// (#28): Go chose its compatibility rewrites with it. Config entries are refused at
    /// load ([`cpa_core::config`]); a Home credential's `credential_options` arrive at run
    /// time, so the request is refused here, scoped to the credential, before anything is
    /// sent.
    pub fn refuse_compat(&self, req: &ExecRequest, credential: &Credential) -> Result<(), ExecError> {
        if !self.is_compat(req) {
            return Ok(());
        }
        Err(ExecError::local(
            500,
            FailureScope::Credential,
            format!(
                "codex credential {}: model {} is marked is-compat, which Codex no longer supports: Codex requests are not rewritten for compatible upstreams; remove the flag from the credential's model options",
                credential.id,
                req.model.trim()
            ),
        ))
    }

    /// `multimedia.disable-image-generation` on a Codex body. Go stripped the client's
    /// `image_generation` tool (or a `tool_choice` naming it) in its payload step; no
    /// Codex route edits a body (#28), so the request is refused before anything is sent,
    /// and the operator's setting still keeps the tool from the provider.
    pub fn refuse_image_tool(&self, request_path: &str, body: &[u8]) -> Result<(), ExecError> {
        match cpa_common::payload::disabled_image_tool(self.image_generation, request_path, body) {
            None => Ok(()),
            Some(field) => Err(image_tool_refused(field)),
        }
    }

    /// `resolveCodexModelIsCompat`: the model dispatch bound to the attempt, else the
    /// credential's config entry, where the first model whose name or alias equals the
    /// base or requested model decides; none means not compat. Without config models,
    /// `CodexAPIKeyModelIsCompat` for the base, then the requested model: in Home mode
    /// the credential's `credential_options` entry for that model.
    pub fn is_compat(&self, req: &ExecRequest) -> bool {
        if let Some(resolved) = &req.resolved_model {
            return resolved.is_compat();
        }
        let fold = |a: &str, b: &str| !b.is_empty() && a.go_eq_fold(b);
        let requested = req.model.trim();
        let base = base_model(&req.model);
        let target = base.trim();
        if self.key_models.is_empty() {
            let home = |model: &str| {
                self.home
                    .as_ref()
                    .and_then(|c| cpa_home::dispatch::credential_model_options(c, model, model))
                    .is_some_and(|options| options.get("is-compat") == Some(&serde_json::Value::Bool(true)))
            };
            return home(target) || home(&req.model);
        }
        self.key_models
            .iter()
            .find(|m| {
                let field = |k: &str| m.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().trim();
                let (name, alias) = (field("name"), field("alias"));
                fold(name, target) || fold(alias, target) || fold(name, requested) || fold(alias, requested)
            })
            .and_then(|m| m.get("is-compat"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    pub fn from(cfg: &Config) -> Self {
        // Shared settings (`upstream.codex`); the header defaults stay OAuth-only.
        let codex = |k: &str| setting(cfg, &["upstream", "codex", k]);
        let flag = |k: &str| codex(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let text = |path: &[&str]| {
            setting(cfg, path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        Self {
            native_session_headers: flag("disable-codex-cloaking"),
            bootstrap_buffering: flag("stream-bootstrap-buffering"),
            bootstrap_timeout: codex("stream-bootstrap-timeout")
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .or_else(|| v.as_i64().map(|n| n.to_string()))
                })
                .and_then(|s| go_duration(&s))
                .filter(|d| !d.is_zero()),
            model_level_cooling: flag("model-level-cooling"),
            default_beta_features: text(&["oauth", "providers", "codex", "header-defaults", "beta-features"]),
            image_generation: cpa_common::payload::Rules::of(cfg).image_generation,
            key_models: Vec::new(),
            home: None,
            response_steering: response_steering(cfg),
            image_base_model: text(&["multimedia", "gpt-image-2-base-model"]),
        }
    }
}

/// `StreamBootstrapTimeoutDuration`: Go durations, bare seconds, or off words.
/// Anything else is Go's default of 0, meaning no timeout.
fn go_duration(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw == "0"
        || ["none", "unlimited", "disabled", "off", "never"]
            .iter()
            .any(|w| raw.eq_ignore_ascii_case(w))
    {
        return None;
    }
    if let Some(nanos) = cpa_core::config::parse_duration(raw).filter(|n| *n >= 0) {
        return Some(Duration::from_nanos(nanos as u64));
    }
    // strconv.Atoi, capped where seconds still fit a time.Duration.
    raw.parse::<i64>()
        .ok()
        .filter(|secs| (0..=i64::MAX / 1_000_000_000).contains(secs))
        .map(|secs| Duration::from_secs(secs as u64))
}

/// The credential fields the Codex executor reads (`codexCreds`, `codexAuthUsesAPIKey`).
pub(crate) struct View<'a> {
    pub credential: &'a Credential,
    pub token: &'a str,
    pub api_key: bool,
    /// `codexUseSIWC`: the credential sends its Sign in with ChatGPT token (7b0319fb).
    pub siwc: bool,
    pub base_url: &'a str,
    /// Effective proxy (`effectiveProxyURL`): the credential's, then `requests.proxy-url`.
    pub proxy: crate::proxy::Proxy,
    /// `$CPA-SESSION-ID`: the request's explicit session, never derived fallbacks.
    pub session: Option<String>,
    /// `oauth.providers.codex.chatgpt-keep-alive` (cliproxy-rs addition, off by default):
    /// keep idle chatgpt.com connections instead of Go's connection per request. Read
    /// from the config snapshot of each request, so a reload applies to the next one.
    pub chatgpt_keep_alive: bool,
    /// `requests.tls-handshake-timeout-seconds` for chatgpt.com ([`crate::tls::handshake_timeout`]).
    pub handshake: Option<std::time::Duration>,
}

impl<'a> View<'a> {
    /// The view one request uses, with its effective proxy.
    pub fn for_request(credential: &'a Credential, cfg: &Config) -> Self {
        Self {
            proxy: crate::proxy::Proxy::effective(credential, cfg),
            chatgpt_keep_alive: chatgpt_keep_alive(cfg),
            handshake: crate::tls::handshake_timeout(cfg),
            ..Self::new(credential)
        }
    }

    /// Binds the request's explicit session for `$CPA-SESSION-ID` custom headers.
    pub fn with_session(mut self, session: Option<String>) -> Self {
        self.session = session;
        self
    }

    pub fn new(credential: &'a Credential) -> Self {
        let attr = |k: &str| credential.attributes.get(k).map(String::as_str).unwrap_or_default();
        let api_key = attr("api_key");
        let siwc = crate::codex_siwc::enabled(credential);
        // `codexCreds`: a Sign in with ChatGPT credential sends its nested token to the
        // OpenAI API unless `base_url` names another endpoint.
        let (token, base_url) = if siwc {
            let base = attr("base_url");
            (
                crate::codex_siwc::access_token(credential),
                if base.trim().is_empty() {
                    crate::codex_siwc::RESOURCE
                } else {
                    base
                },
            )
        } else {
            let token = if api_key.is_empty() {
                credential.str("access_token").unwrap_or_default()
            } else {
                api_key
            };
            let base = match attr("base_url") {
                "" => DEFAULT_BASE_URL,
                url => url,
            };
            (token, base)
        };
        let kind = attr("auth_kind").trim().to_ascii_lowercase().replace(['_', '-'], "");
        let is_api_key = kind == "apikey" || (kind.is_empty() && !api_key.trim().is_empty());
        Self {
            credential,
            token,
            api_key: is_api_key,
            siwc,
            base_url: base_url.trim_end_matches('/'),
            proxy: crate::proxy::Proxy::Inherit,
            session: None,
            chatgpt_keep_alive: false,
            handshake: None,
        }
    }

    pub fn attr(&self, key: &str) -> &str {
        self.credential
            .attributes
            .get(key)
            .map(String::as_str)
            .unwrap_or_default()
    }

    /// Go `isCodexCloakingDisabled`, which now only decides
    /// [`Settings::native_session_headers`]: the attribute (config synthesis copies the
    /// matching `codex-api-key` entry's `disable-codex-cloaking` into it), then the
    /// setting. Neither can change the request identity.
    pub fn native_session_headers(&self, settings: &Settings) -> bool {
        go_bool(self.attr("codex_disable_cloaking")).unwrap_or(settings.native_session_headers)
    }

    /// `codexWebsocketsEnabled`: attribute first, then metadata.
    pub fn websockets(&self) -> bool {
        match self.attr("websockets").trim() {
            "" => match self.credential.metadata.get("websockets") {
                Some(serde_json::Value::Bool(b)) => *b,
                Some(serde_json::Value::String(s)) => go_bool(s).unwrap_or(false),
                _ => false,
            },
            raw => go_bool(raw).unwrap_or(false),
        }
    }
}

/// Go `strconv.ParseBool`.
fn go_bool(s: &str) -> Option<bool> {
    match s.trim() {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// `thinking.ParseSuffix(model).ModelName`.
pub(crate) fn base_model(model: &str) -> String {
    cpa_common::thinking::parse_suffix(model).model_name
}

pub(crate) fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
}

/// `util.IsCodexResponsesLiteRequest`. The header is decoded as UTF-8 rather than through
/// [`header`], so Go's `TrimSpace` also strips non-ASCII spaces such as U+00A0.
pub(crate) fn is_lite(body: &str, headers: &HeaderMap) -> bool {
    let lite = headers
        .get(LITE_HEADER)
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        .unwrap_or_default();
    if lite.trim().go_eq_fold("true") {
        return true;
    }
    let v = gjson::get(
        body,
        "client_metadata.ws_request_header_x_openai_internal_codex_responses_lite",
    );
    v.kind() == Kind::True || (v.kind() == Kind::String && v.str().trim().eq_ignore_ascii_case("true"))
}

/// Codex/Responses formats on both sides ([`responses_pair`]).
pub(crate) fn responses_formats(req: &ExecRequest) -> bool {
    responses_pair(req.source_format, req.response_format)
}

/// Whether a client and a response format are both Responses, in either spelling
/// (`openai-response` or `codex`): the formats of the native route. The server uses the
/// same test to keep a request's own bytes for that route
/// ([`crate::codex::CodexExecutor::responses_pair`]).
pub(crate) fn responses_pair(source: Format, response: Format) -> bool {
    let codexish = |f: Format| matches!(f, Format::Codex | Format::OpenAIResponse);
    codexish(source) && codexish(response)
}

/// `helps.IsNativeCodexRequest`: Codex/Responses formats on both sides plus the lite marker.
pub(crate) fn is_native(req: &ExecRequest) -> bool {
    responses_formats(req) && is_lite(&String::from_utf8_lossy(&req.body), &req.headers)
}

/// An official Codex client (Go `isCodexResponsesClientRequest`) reading Responses events
/// itself: it gets an upstream context-window rejection as the `response.failed` Codex
/// compacts on (`cpa_common::codex_client::context_window_failure_event`).
pub(crate) fn codex_client(req: &ExecRequest) -> bool {
    responses_formats(req) && cpa_common::codex_client::is_codex_responses_client(&req.headers)
}

/// The refusal of an `image_generation` tool that `multimedia.disable-image-generation`
/// disables; `field` is `tools` or `tool_choice`.
pub(crate) fn image_tool_refused(field: &str) -> ExecError {
    ExecError::local(
        400,
        FailureScope::Request,
        format!(
            "image generation is disabled on this proxy (multimedia.disable-image-generation): remove the `image_generation` tool from `{field}`"
        ),
    )
}

/// `uuid.NewString()`: a random v4 UUID.
pub(crate) fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    uuid::Builder::from_random_bytes(bytes).into_uuid().to_string()
}

/// UUIDv5(OID, "cli-proxy-api\0codex\0<kind>\0<id>") (`stableProviderSessionUUID`).
pub(crate) fn provider_session_uuid(kind: &str, id: &str) -> Option<String> {
    let id = id.trim();
    (!id.is_empty()).then(|| {
        uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("cli-proxy-api\0codex\0{kind}\0{id}").as_bytes(),
        )
        .to_string()
    })
}

/// `ProviderSessionUUID`: the long-lived execution session (the downstream WebSocket
/// connection) when there is one, else the derived session identity.
pub(crate) fn session_uuid(req: &ExecRequest, ws_session: Option<&str>) -> Option<String> {
    let execution = ws_session
        .or(req.execution_session.as_deref())
        .filter(|id| !id.trim().is_empty());
    match execution {
        Some(id) => provider_session_uuid("execution-session", id),
        None => req
            .derived_session
            .as_deref()
            .and_then(|id| provider_session_uuid("derived-session", id)),
    }
}

/// `helps.ClaudeCodePromptCache`: one deterministic key per Claude Code agent and model.
fn claude_code_prompt_cache(model: &str, payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let model = model.trim();
    let scope = crate::replay::claude_code_session(payload, headers)?;
    (!model.is_empty()).then(|| {
        let identity = format!("cli-proxy-api:codex:claude-code\0{model}\0{scope}");
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, identity.as_bytes()).to_string()
    })
}

/// The prompt cache identity [`prompt_cache`] would write, without writing it: the
/// native HTTP route sends it only in headers (#26 owns which).
pub(crate) fn prompt_cache_id(
    req: &ExecRequest,
    body: &str,
    ws_session: Option<&str>,
    websocket: bool,
) -> Option<String> {
    let original = String::from_utf8_lossy(&req.body);
    let client_key = gjson::get(&original, "prompt_cache_key");
    let mut id = match req.source_format {
        Format::Claude => {
            let mut model = gjson::get(body, "model").str().trim().to_owned();
            if model.is_empty() {
                model = base_model(&req.model);
            }
            claude_code_prompt_cache(&model, &req.body, &req.headers).unwrap_or_default()
        }
        Format::OpenAIResponse if client_key.exists() => client_key.str().to_owned(),
        Format::OpenAI if client_key.exists() && !websocket => client_key.str().trim().to_owned(),
        _ => String::new(),
    };
    if id.is_empty() {
        id = session_uuid(req, ws_session).unwrap_or_default();
    }
    if id.is_empty() && !websocket && req.source_format == Format::OpenAI && !req.caller.principal.trim().is_empty() {
        id = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("cli-proxy-api:codex:prompt-cache:{}", req.caller.principal.trim()).as_bytes(),
        )
        .to_string();
    }
    (!id.is_empty()).then_some(id)
}

fn set(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
        headers.insert(name, value);
    }
}

/// `misc.EnsureHeader`: the client's value wins, then what is already set.
fn ensure_from_client(headers: &mut HeaderMap, client: &HeaderMap, name: &str) {
    let value = header(client, name).trim();
    if !value.is_empty() {
        set(headers, name, value);
    }
}

/// `util.ApplyCustomHeadersFromAttrs` (`cpa_common::headers`) with the view's explicit
/// `$CPA-SESSION-ID`, minus the operator headers the identity policy refuses
/// ([`identity::permitted_attributes`]): no client identity, no caller-chosen credential
/// and no copied client key.
pub(crate) fn custom_headers(view: &View<'_>, client: &HeaderMap) -> Vec<(String, String)> {
    cpa_common::headers::custom_headers(
        &identity::permitted_attributes(view.credential),
        client,
        view.session.as_deref(),
    )
}

/// `applyModelHeaderOverrides`: models.json `config.override_header` for the model,
/// except identity and credential headers ([`identity::model_override_permitted`]). The
/// pinned catalog gives `gpt-5.6-luna` a `codex-tui` macOS identity, which #26 removes.
fn model_overrides(model: &str) -> Vec<(String, String)> {
    let Some(info) = cpa_core::registry::pinned().lookup(model) else {
        return Vec::new();
    };
    info.raw
        .get("config")
        .and_then(|c| c.get("override_header"))
        .and_then(serde_json::Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !k.trim().is_empty() && identity::model_override_permitted(k))
                .filter_map(|(k, v)| Some((k.trim().to_owned(), v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// HTTP upstream headers (`applyCodexHeadersFromSources`, routing hint, model overrides).
/// `origin` decides whose identity they carry ([`crate::codex_identity`]).
pub(crate) fn http_headers(
    view: &View<'_>,
    client: &HeaderMap,
    body: &str,
    model: &str,
    cache_id: Option<&str>,
    stream: bool,
    origin: Origin,
) -> HeaderMap {
    let mut h = source_headers(view, client, cache_id, stream, origin);
    routing_hint(&mut h, view, client, body, model);
    model_header_overrides(&mut h, model);
    h
}

/// Images API upstream headers: `applyCodexHeaders` and `applyModelHeaderOverrides`,
/// without the routing hint. The proxy builds every Images request, so it carries the
/// proxy's identity ([`Origin::Proxy`]).
pub(crate) fn image_headers(
    view: &View<'_>,
    client: &HeaderMap,
    model: &str,
    cache_id: Option<&str>,
    stream: bool,
) -> HeaderMap {
    let mut h = source_headers(view, client, cache_id, stream, Origin::Proxy);
    model_header_overrides(&mut h, model);
    h
}

/// `cacheHelper`'s `Session-Id`, then `applyCodexHeadersFromSources`.
fn source_headers(
    view: &View<'_>,
    client: &HeaderMap,
    cache_id: Option<&str>,
    stream: bool,
    origin: Origin,
) -> HeaderMap {
    if view.siwc {
        return siwc_http_headers(view, client, cache_id, stream, origin);
    }
    let mut h = HeaderMap::new();
    if let Some(id) = cache_id {
        set(&mut h, "session-id", id);
    }
    set(&mut h, "content-type", "application/json");
    if !view.token.trim().is_empty() {
        set(&mut h, "authorization", &format!("Bearer {}", view.token));
    }
    for name in [
        "x-codex-beta-features",
        "version",
        "x-codex-turn-metadata",
        "x-codex-turn-state",
        "x-client-request-id",
        "x-codex-window-id",
        "thread-id",
        "session-id",
        LITE_HEADER,
    ] {
        ensure_from_client(&mut h, client, name);
    }
    set(&mut h, "user-agent", identity::user_agent(origin, client));
    set(
        &mut h,
        "accept",
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
    );
    set(&mut h, "connection", "Keep-Alive");
    apply_identity(&mut h, view, client, origin);
    h
}

/// The Sign in with ChatGPT branch of `applyCodexHeadersFromSources`: the session as
/// `session_id` and the client request id, the policy's User-Agent and operator headers;
/// no originator, account id or client Codex headers. Go sends
/// `<signin-with-chatgpt-identifier> (<GOOS>; <GOARCH>)`, an operator-chosen identity;
/// the identifier now only labels the consent screen (`agent_name_hint`, #26).
fn siwc_http_headers(
    view: &View<'_>,
    client: &HeaderMap,
    cache_id: Option<&str>,
    stream: bool,
    origin: Origin,
) -> HeaderMap {
    let mut h = HeaderMap::new();
    set(&mut h, "content-type", "application/json");
    if !view.token.trim().is_empty() {
        set(&mut h, "authorization", &format!("Bearer {}", view.token));
    }
    if let Some(id) = cache_id.map(str::trim).filter(|id| !id.is_empty()) {
        set(&mut h, "session_id", id);
        set(&mut h, "x-client-request-id", id);
    }
    set(&mut h, "user-agent", identity::user_agent(origin, client));
    set(
        &mut h,
        "accept",
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
    );
    set(&mut h, "connection", "Keep-Alive");
    for (name, value) in custom_headers(view, client) {
        set(&mut h, &name, &value);
    }
    h
}

/// Originator, the selected credential's account header and operator headers, shared by
/// HTTP and WebSocket. An OAuth credential always carries an originator, as in Go: the
/// caller's on a native exchange, else the proxy's ([`identity::originator`]).
fn apply_identity(h: &mut HeaderMap, view: &View<'_>, client: &HeaderMap, origin: Origin) {
    if let Some(originator) = identity::originator(origin, client, !view.api_key) {
        set(h, "originator", originator);
    }
    if !view.api_key
        && let Some(account) = view.credential.str("account_id")
    {
        set(h, "chatgpt-account-id", account.trim());
    }
    for (name, value) in custom_headers(view, client) {
        set(h, &name, &value);
    }
}

/// `applyCodexRoutingHint`: `model=<slug>[;tier=<service_tier>]` for OAuth credentials.
fn routing_hint(h: &mut HeaderMap, view: &View<'_>, client: &HeaderMap, body: &str, model: &str) {
    if view.api_key || view.siwc {
        return;
    }
    h.remove(ROUTING_HINT);
    let operator = custom_headers(view, client)
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(ROUTING_HINT))
        .map(|(_, v)| v);
    if let Some(value) = operator.filter(|v| !v.trim().is_empty()) {
        set(h, ROUTING_HINT, value.trim());
        return;
    }
    let model = model.trim();
    if model.is_empty() {
        return;
    }
    let mut hint = format!("model={model}");
    let tier = gjson::get(body, "service_tier");
    if tier.kind() == Kind::String && !tier.str().trim().is_empty() {
        hint.push_str(";tier=");
        hint.push_str(tier.str().trim());
    }
    set(h, ROUTING_HINT, &hint);
}

fn model_header_overrides(h: &mut HeaderMap, model: &str) {
    let overrides = model_overrides(model);
    if overrides.is_empty() {
        return;
    }
    for (k, v) in overrides {
        set(h, &k, &v);
    }
    let has_session = ["session-id", "session_id"]
        .iter()
        .any(|n| !header(h, n).trim().is_empty());
    if header(h, "user-agent").contains("Mac OS") && !has_session {
        set(h, "session_id", &random_uuid());
    }
}

/// WebSocket handshake headers (`applyCodexPromptCacheHeadersWithContext`,
/// `applyCodexWebsocketHeaders`, routing hint, model overrides). `origin` decides whose
/// identity they carry ([`crate::codex_identity`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn ws_headers(
    view: &View<'_>,
    settings: &Settings,
    client: &HeaderMap,
    body: &str,
    model: &str,
    cache_id: Option<&str>,
    native: bool,
    origin: Origin,
) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Some(id) = cache_id {
        set(&mut h, "session_id", id);
        set(&mut h, "conversation_id", id);
    }
    if !view.token.trim().is_empty() {
        set(&mut h, "authorization", &format!("Bearer {}", view.token));
    }
    if view.siwc {
        siwc_ws_headers(&mut h, view, client, origin);
        model_header_overrides(&mut h, model);
        return h;
    }
    let default_beta = if view.api_key {
        ""
    } else {
        settings.default_beta_features.as_str()
    };
    // ensureHeaderWithPriority: existing, client, config.
    for value in [header(client, "x-codex-beta-features"), default_beta] {
        if !value.trim().is_empty() {
            set(&mut h, "x-codex-beta-features", value.trim());
            break;
        }
    }
    for name in [
        "x-codex-turn-state",
        "x-codex-turn-metadata",
        "x-client-request-id",
        "x-responsesapi-include-timing-metrics",
        "version",
    ] {
        ensure_from_client(&mut h, client, name);
    }
    if native {
        ensure_from_client(&mut h, client, LITE_HEADER);
    }
    set(&mut h, "user-agent", identity::user_agent(origin, client));
    let mut beta = header(client, "openai-beta").trim().to_owned();
    if !beta.contains("responses_websockets=") {
        beta = WS_BETA.to_owned();
    }
    set(&mut h, "openai-beta", &beta);
    // ensureCodexWebsocketSessionHeader: existing, client, then a fresh id for Mac OS UAs.
    let mut session = header(&h, "session_id").trim().to_owned();
    if session.is_empty() {
        session = ["session-id", "session_id"]
            .iter()
            .map(|n| header(client, n).trim())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
            .to_owned();
    }
    if session.is_empty() && header(&h, "user-agent").contains("Mac OS") {
        session = random_uuid();
    }
    if !session.is_empty() {
        set(&mut h, "session_id", &session);
    }
    h.remove("session-id");
    if native && view.native_session_headers(settings) {
        h.remove("session_id");
        h.remove("conversation_id");
        for name in [
            "session-id",
            "session_id",
            "conversation_id",
            "thread-id",
            ROUTING_HINT,
            "x-codex-window-id",
        ] {
            for value in client.get_all(name) {
                h.append(HeaderName::from_static(name), value.clone());
            }
        }
    }
    apply_identity(&mut h, view, client, origin);
    routing_hint(&mut h, view, client, body, model);
    model_header_overrides(&mut h, model);
    h
}

/// The Sign in with ChatGPT branch of `applyCodexWebsocketHeaders`: no conversation
/// header, the session (prompt cache or client) also as the client request id, the
/// policy's User-Agent (see [`siwc_http_headers`]), the client's WebSocket beta header
/// when it names one, then operator headers.
fn siwc_ws_headers(h: &mut HeaderMap, view: &View<'_>, client: &HeaderMap, origin: Origin) {
    h.remove("conversation_id");
    let existing = header(h, "session_id").trim().to_owned();
    let session = if existing.is_empty() {
        ["session-id", "session_id"]
            .iter()
            .map(|n| header(client, n).trim())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
            .to_owned()
    } else {
        existing
    };
    if session.is_empty() {
        ensure_from_client(h, client, "x-client-request-id");
    } else {
        set(h, "session_id", &session);
        set(h, "x-client-request-id", &session);
    }
    set(h, "user-agent", identity::user_agent(origin, client));
    let beta = header(client, "openai-beta").trim();
    let beta = if beta.contains("responses_websockets=") {
        beta
    } else {
        WS_BETA
    };
    set(h, "openai-beta", beta);
    for (name, value) in custom_headers(view, client) {
        set(h, &name, &value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go's `NormalizeCodexToolIntegerTypes` vectors (tests/fixtures/codex_tool_schema_go.json,
    /// recorded from v8.0.35): a Codex client's `number` tool parameters become `integer`
    /// for the providers that are not Codex (`cpa_common::payload`). The fixture's
    /// `schemas` vectors replayed the Codex executor's own schema normalization, which #28
    /// deleted: a Codex request keeps the caller's schemas.
    #[test]
    fn codex_tool_integer_types_match_go() {
        let go: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/codex_tool_schema_go.json")).unwrap();
        let expected = |case: &serde_json::Value| case.get("out").unwrap_or(&case["in"]).as_str().unwrap().to_owned();
        let integers = go["integers"].as_array().unwrap();
        assert!(!integers.is_empty());
        for case in integers {
            let mut headers = HeaderMap::new();
            if let Some(ua) = case["ua"].as_str() {
                headers.insert(http::header::USER_AGENT, ua.parse().unwrap());
            }
            let out = cpa_common::payload::normalize_codex_tool_integer_types(
                case["in"].as_str().unwrap().as_bytes(),
                &headers,
            );
            assert_eq!(
                String::from_utf8(out.clone()).unwrap(),
                expected(case),
                "{} {:?}",
                case["name"],
                case["ua"]
            );
            // Go's tests also require a second pass to leave the output alone.
            let again = cpa_common::payload::normalize_codex_tool_integer_types(&out, &headers);
            assert_eq!(again, out, "{} is not idempotent", case["name"]);
        }
    }

    /// Go `TestHomeCodexCompatUsesSelectedCredentialOptions`: without a binding or config
    /// models, a Home credential's `credential_options` decide is-compat for the upstream
    /// name, its alias and its suffixed form; another credential and standalone mode do
    /// not see them.
    #[test]
    fn home_codex_is_compat_reads_the_credential_options() {
        let mut credential = Credential {
            id: "c1".into(),
            provider: "codex".into(),
            source: cpa_core::credential::Source::File("c1".into()),
            disabled: false,
            label: String::new(),
            attributes: Default::default(),
            metadata: serde_json::json!({"credential_options": {"models": [{"name": "upstream", "alias": "alias", "is-compat": true}]}})
                .as_object()
                .cloned()
                .unwrap(),
            revision: 0,
        };
        let settings = |c: &Credential| Settings {
            home: c
                .attributes
                .contains_key(cpa_core::config::credentials::HOME_PROVIDER)
                .then(|| c.clone()),
            ..Settings::default()
        };
        let request = |model: &str| ExecRequest {
            operation: cpa_core::exec::Operation::Generate,
            source_format: Format::Codex,
            response_format: Format::Codex,
            requested_model: model.into(),
            model: model.into(),
            original_body: Default::default(),
            body: Default::default(),
            stream: false,
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            request_path: String::new(),
            headers: Default::default(),
            caller: cpa_core::exec::Caller {
                principal: String::new(),
                source: "",
                provider: String::new(),
            },
            resolved_model: None,
            usage: Default::default(),
        };
        credential
            .attributes
            .insert(cpa_core::config::credentials::HOME_PROVIDER.into(), "codex".into());
        for model in ["upstream", "alias", "upstream(high)"] {
            assert!(settings(&credential).is_compat(&request(model)), "{model}");
        }
        let mut other = credential.clone();
        other.metadata.clear();
        assert!(!settings(&other).is_compat(&request("upstream")));
        credential.attributes.clear();
        assert!(!settings(&credential).is_compat(&request("upstream")), "standalone");

        // Config models compare names and aliases with Go's EqualFold: İ does not fold to
        // i, while σ and ς fold together.
        let keyed = |name: &str| Settings {
            key_models: vec![serde_json::json!({"name": name, "is-compat": true})],
            ..Settings::default()
        };
        assert!(!keyed("mİ").is_compat(&request("mi")), "dotted İ is not i");
        assert!(keyed("ος").is_compat(&request("ΟΣ")), "final sigma folds");
    }

    #[test]
    fn durations_follow_go_parse_duration() {
        assert_eq!(go_duration("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(go_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(go_duration("off"), None);
        assert_eq!(go_duration("soon"), None);
        assert_eq!(go_duration("+2s"), Some(Duration::from_secs(2)));
        assert_eq!(go_duration("3\u{3bc}s"), Some(Duration::from_micros(3)));
        assert_eq!(go_duration("-1s"), None);
        assert_eq!(go_duration("+15"), Some(Duration::from_secs(15)));
    }

    /// `chatgpt-keep-alive` is off unless the config turns it on, in the v8 layout or the
    /// flat one (whose `codex:` section moves to `oauth.providers.codex`), and YAML 1.1
    /// spellings decode as Go decodes a bool field.
    #[test]
    fn chatgpt_keep_alive_is_off_unless_configured() {
        let cases = [
            ("", false),
            (
                "oauth:\n  providers:\n    codex:\n      disable-codex-cloaking: true\n",
                false,
            ),
            (
                "oauth:\n  providers:\n    codex:\n      chatgpt-keep-alive: false\n",
                false,
            ),
            (
                "oauth:\n  providers:\n    codex:\n      chatgpt-keep-alive: true\n",
                true,
            ),
            (
                "oauth:\n  providers:\n    codex:\n      chatgpt-keep-alive: yes\n",
                true,
            ),
            ("codex:\n  chatgpt-keep-alive: true\n", true),
        ];
        for (yaml, want) in cases {
            let cfg = Config::parse(yaml).unwrap();
            assert_eq!(chatgpt_keep_alive(&cfg), want, "{yaml:?}");
        }
    }
}
