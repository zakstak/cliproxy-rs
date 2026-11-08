//! Codex Images API (codex_openai_images.go): `/v1/images/generations` and
//! `/v1/images/edits` on Codex credentials.
//!
//! A gpt-image-* model goes straight to the backend's own `/images/*` endpoint
//! ("direct"). Any other model becomes a Responses request that forces the
//! `image_generation` tool on the images main model (`multimedia.gpt-image-2-base-model`
//! when it names a `gpt-` model, else gpt-5.4-mini). The tool calls of
//! `response.completed` become an Images API response, or `<prefix>.partial_image` and
//! `<prefix>.completed` SSE frames when streaming.

use cpa_core::gostr::GoStr;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::rawjson::{self, Kind};
use base64::Engine;
use bytes::Bytes;
use cpa_common::codex_body::{self as body, BodyError, CodexBody, Member};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;

use crate::codex::{CodexExecutor, events, explicit_session, report_request};
use crate::codex_capture::Wire;
use crate::codex_json::{delete, set_bool_if_different, set_raw, set_str, set_str_if_different};
use crate::codex_request::{self as request, Settings, View};
use crate::codex_response::{self as response, OutputItems};
use crate::multipart::{self as multipart, Form, MediaType, Part};
use crate::openai_http::{plain_err, status_err};

const GENERATIONS: &str = "/v1/images/generations";
const EDITS: &str = "/v1/images/edits";
/// `codexOpenAIImagesMainModel`.
const MAIN_MODEL: &str = "gpt-5.4-mini";
/// `codexDefaultImageToolModel`.
const DEFAULT_TOOL_MODEL: &str = "gpt-image-2";
/// Models the backend serves on its own Images endpoints (`codexIsDirectOpenAIImageModel`).
const DIRECT_MODELS: [&str; 7] = [
    "gpt-image-1.5",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
    "gpt-image-2.6-flare",
    "gpt-image-2.6-sunburst",
];
/// The Images API's source format name in payload rules and thinking.
const SOURCE: &str = "openai-image";

/// `codexOpenAIImagePreparedRequest`.
struct Prepared {
    body: String,
    url_format: bool,
    /// `image_generation` or `image_edit`: the SSE event name prefix.
    prefix: &'static str,
}

/// One completed `image_generation_call` (`codexImageCallResult`).
#[derive(Default, Clone)]
struct ImageCall {
    result: String,
    revised_prompt: String,
    output_format: String,
    size: String,
    background: String,
    quality: String,
}

impl CodexExecutor {
    /// The Images API on a Codex credential (`executeOpenAIImage`,
    /// `executeOpenAIImageStream`). `request_path` is the inbound route; `req.stream`
    /// asks for SSE.
    pub async fn images(
        &self,
        credential: &Credential,
        req: ExecRequest,
        request_path: &str,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        crate::codex_siwc::require_token(credential)?;
        crate::codex_identity::check_credential(credential)?;
        let view = View::for_request(credential, cfg).with_session(explicit_session(&req));
        let settings = Settings::scoped(cfg, &view);
        let path = request_path.trim();
        // The client's JSON is read once, through the Codex request boundary (#22); a
        // multipart edit is read as a form by the path that sends it.
        let raw = req.body.clone();
        let json = if is_form(&req, path) {
            None
        } else {
            Some(ImagesJson::parse(&raw)?)
        };
        let payload_model = json.as_ref().and_then(ImagesJson::model);
        // A Sign in with ChatGPT credential has no direct Images endpoint.
        match (
            direct_model(&req, payload_model.as_deref(), &settings),
            direct_endpoint(path),
        ) {
            (Some(model), Some(endpoint)) if !view.siwc => {
                self.direct(&view, &settings, req, json.as_ref(), endpoint, &model)
                    .await
            }
            _ => self.tool(&view, &settings, req, json.as_ref(), path).await,
        }
    }

    /// `executeDirectOpenAIImage` / `executeDirectOpenAIImageStream`: the request (JSON, or
    /// a multipart edit as JSON) goes to the backend's Images endpoint; the response
    /// passes through.
    async fn direct(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        json: Option<&ImagesJson<'_>>,
        endpoint: &str,
        model: &str,
    ) -> Result<ExecResponse, ExecError> {
        let (body, content_type) = match json {
            Some(json) => (
                direct_json_payload(&req.body, json, model, req.stream),
                "application/json".to_owned(),
            ),
            None => {
                let content_type = request::header(&req.headers, "content-type");
                direct_edit_form(&req.body, model, content_type, req.stream)?
            }
        };
        if req.usage.enabled() {
            req.usage.request(Format::OpenAI, &body);
        }
        // No synthesized prompt-cache key in the body (#27); the session identity reaches
        // the headers only, and #26 owns those.
        let cache = request::session_uuid(&req, None);
        // No input-ID rewrite or payload rule either (#27): the body is the client's
        // Images request with only its model and `stream` routed.
        // `applyCodexDirectImageHeaders` drops the client's User-Agent; `image_headers`
        // never forwards it (the proxy builds every Images request).
        let mut headers = request::image_headers(view, &req.headers, model, cache.as_deref(), req.stream);
        if !content_type.is_empty()
            && let Ok(value) = http::HeaderValue::from_str(&content_type)
        {
            headers.insert(http::header::CONTENT_TYPE, value);
        }
        let url = format!("{}{endpoint}", view.base_url);
        let wire = Wire::new(req.capture(), view.credential);
        wire.request(&url, &headers, &body);
        let upstream = match self.post(view, &url, &headers, body).await {
            Ok(upstream) => upstream,
            Err(error) => {
                wire.exec_error(&error);
                return Err(error);
            }
        };
        wire.metadata(upstream.status, &upstream.headers);
        self.quota().observe(&view.credential.id, model, &upstream.headers);
        let status = upstream.status;
        let mut headers = upstream.headers;
        headers.remove(http::header::CONTENT_ENCODING);
        headers.remove(http::header::CONTENT_LENGTH);
        if !(200..300).contains(&status) {
            // Both direct modes record and return a read error of the error body.
            let body = match crate::proxy::read_all(upstream.body, crate::proxy::MAX_ERROR_BODY, false).await {
                Ok(body) => body,
                Err(error) => {
                    wire.exec_error(&error);
                    return Err(error);
                }
            };
            wire.chunk(&body);
            return Err(response::status_error(
                status,
                &body,
                headers,
                settings.model_level_cooling,
            ));
        }
        if !req.stream {
            let data = match crate::proxy::read_all(upstream.body, usize::MAX, false).await {
                Ok(data) => data,
                Err(error) => {
                    wire.exec_error(&error);
                    return Err(error);
                }
            };
            wire.chunk(&data);
            if req.usage.enabled() {
                req.usage.response_body(Format::OpenAI, &data);
            }
            return Ok(ExecResponse {
                status,
                headers,
                body: ResponseBody::Buffered(data),
            });
        }
        let usage = req.usage.clone();
        let stream = upstream.body.inspect(move |chunk| {
            // Go records each body read as it is.
            match chunk {
                Ok(chunk) => wire.chunk(chunk),
                Err(error) => wire.exec_error(error),
            }
            if let Ok(chunk) = chunk
                && usage.enabled()
            {
                // Go splits each read on newlines (`ObserveOpenAIStream`).
                for line in chunk.split(|b| *b == b'\n') {
                    let line = line.trim_ascii();
                    let data = line.strip_prefix(b"data:").map_or(line, <[u8]>::trim_ascii);
                    if !data.is_empty() {
                        usage.response_line(Format::OpenAI, data);
                    }
                }
            }
        });
        Ok(ExecResponse {
            status,
            headers,
            body: ResponseBody::Stream(stream.boxed()),
        })
    }

    /// The Responses path: a forced `image_generation` tool call on the main model.
    async fn tool(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        json: Option<&ImagesJson<'_>>,
        path: &str,
    ) -> Result<ExecResponse, ExecError> {
        let prepared = prepare(&req, json, path)?;
        let main_model = main_model(&settings.image_base_model);
        let body = tool_body(&prepared.body, main_model)?;
        // No synthesized prompt-cache key in the body (#27); the session identity reaches
        // the headers only, and #26 owns those. The input is the adapter's own.
        let cache = request::session_uuid(&req, None);
        // No input-ID rewrite or payload rule (#27): the body is the adapter's conversion.
        report_request(&req, Format::Codex, &body);
        let headers = request::image_headers(view, &req.headers, main_model, cache.as_deref(), true);
        let url = format!("{}/responses", view.base_url);
        let wire = Wire::new(req.capture(), view.credential);
        let res = self
            .send(
                view,
                settings,
                url,
                headers,
                body,
                &wire,
                crate::codex::ErrorBody::Strict,
            )
            .await?;
        let upstream = events(res.body);
        // ponytail: usage lands on the images main model's record; Go also publishes the
        // completion's `tool_usage.image_gen` under the tool model, which `UsageSink`
        // cannot express yet (see `Processor::reporting`).
        if req.stream {
            return Ok(ExecResponse {
                status: res.status,
                headers: res.headers,
                body: ResponseBody::Stream(frames(upstream, prepared, req.usage.clone(), wire)),
            });
        }
        // Go reads the whole body (`io.ReadAll`) and records it before parsing.
        let mut upstream = upstream;
        let mut read = Vec::new();
        while let Some(event) = upstream.next().await {
            match event {
                Ok(event) => read.push(event),
                Err(error) => {
                    wire.exec_error(&error);
                    return Err(error);
                }
            }
        }
        if wire.enabled() {
            wire.chunk(&read.concat());
        }
        let mut items = OutputItems::default();
        for event in read {
            for data in data_lines(&event) {
                if req.usage.enabled() {
                    req.usage.response_line(Format::Codex, data.as_bytes());
                }
                match rawjson::get(data, "type").str() {
                    "response.output_item.done" => items.collect(data),
                    "response.completed" => {
                        let (calls, created, usage) = extract(data, &items);
                        if calls.is_empty() {
                            return Err(status_err(502, "upstream did not return image output"));
                        }
                        return Ok(ExecResponse {
                            status: res.status,
                            headers: res.headers,
                            body: ResponseBody::Buffered(Bytes::from(images_response(
                                &calls,
                                created,
                                usage.as_deref(),
                                prepared.url_format,
                            ))),
                        });
                    }
                    _ => {}
                }
            }
        }
        Err(status_err(504, "stream error: stream disconnected before completion"))
    }
}

/// The stream path's frames (`executeOpenAIImageStream`): partial images as they come,
/// then one completed frame per image. A stream that ends without
/// `response.completed` ends quietly, as in Go.
fn frames(upstream: ExecStream, prepared: Prepared, usage: cpa_core::exec::UsageSink, wire: Wire) -> ExecStream {
    struct State {
        upstream: ExecStream,
        prepared: Prepared,
        usage: cpa_core::exec::UsageSink,
        wire: Wire,
        items: OutputItems,
        out: VecDeque<Result<Bytes, ExecError>>,
        done: bool,
    }
    let state = State {
        upstream,
        prepared,
        usage,
        wire,
        items: OutputItems::default(),
        out: VecDeque::new(),
        done: false,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(item) = st.out.pop_front() {
                return Some((item, st));
            }
            if st.done {
                return None;
            }
            let event = match st.upstream.next().await {
                None => return None,
                Some(Err(error)) => {
                    st.done = true;
                    st.wire.exec_error(&error);
                    st.out.push_back(Err(error));
                    continue;
                }
                Some(Ok(event)) => event,
            };
            for raw in event.split(|b| *b == b'\n') {
                let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
                // Go records each scanned line before reading it, and none after the end.
                st.wire.chunk(raw);
                let Some(data) = std::str::from_utf8(raw)
                    .ok()
                    .and_then(|line| line.strip_prefix("data:"))
                    .map(str::trim)
                else {
                    continue;
                };
                if st.usage.enabled() {
                    st.usage.response_line(Format::Codex, data.as_bytes());
                }
                match rawjson::get(data, "type").str() {
                    "response.output_item.done" => st.items.collect(data),
                    "response.image_generation_call.partial_image" => {
                        if let Some(frame) = partial_frame(data, &st.prepared) {
                            st.out.push_back(Ok(Bytes::from(frame)));
                        }
                    }
                    "response.completed" => {
                        st.done = true;
                        let (calls, _, usage) = extract(data, &st.items);
                        if calls.is_empty() {
                            st.out
                                .push_back(Err(status_err(502, "upstream did not return image output")));
                        }
                        for call in &calls {
                            let frame = completed_frame(call, usage.as_deref(), &st.prepared);
                            st.out.push_back(Ok(Bytes::from(frame)));
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    })
    .boxed()
}

/// The `data:` payloads of one SSE event, trimmed (Go reads `data:` lines).
fn data_lines(event: &[u8]) -> Vec<&str> {
    std::str::from_utf8(event)
        .unwrap_or_default()
        .split('\n')
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect()
}

/// `codexDirectOpenAIImageRouteModel`: the client's model (multipart handlers pass it
/// beside the raw form), the JSON payload's model, then the execution model, when one
/// is a gpt-image-* model.
fn direct_route_model(req: &ExecRequest, payload_model: Option<&str>) -> Option<String> {
    [
        req.requested_model.trim(),
        payload_model.unwrap_or_default(),
        req.model.as_str(),
    ]
    .into_iter()
    .map(image_base_model)
    .find(|m| DIRECT_MODELS.contains(&m.as_str()))
}

/// `directOpenAIImageModel`: for a direct route, the resolved upstream model. A name
/// the credential's config lists is sent as it is; otherwise a gpt-image-* model loses
/// its routing prefix and case (51bde0ff).
fn direct_model(req: &ExecRequest, payload_model: Option<&str>, settings: &Settings) -> Option<String> {
    let route = direct_route_model(req, payload_model)?;
    let model = request::base_model(&req.model).trim().to_owned();
    if model.is_empty() {
        return Some(route);
    }
    let configured = settings.key_models.iter().any(|m| {
        let field = |k: &str| m.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().trim();
        let name = Some(field("name"))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| field("alias"));
        request::base_model(name).trim().go_eq_fold(&model)
    });
    if configured {
        return Some(model);
    }
    let base = image_base_model(&model);
    Some(if DIRECT_MODELS.contains(&base.as_str()) {
        base
    } else {
        model
    })
}

/// `codexOpenAIImageBaseModel`: no thinking suffix, no `provider/` prefix, lower case.
fn image_base_model(model: &str) -> String {
    let base = request::base_model(model);
    let mut base = base.trim();
    if let Some(i) = base.rfind('/')
        && i < base.len() - 1
    {
        base = base[i + 1..].trim();
    }
    base.trim().go_lower()
}

/// `codexDirectOpenAIImageEndpoint`.
fn direct_endpoint(path: &str) -> Option<&'static str> {
    if path.ends_with(GENERATIONS) {
        Some("/images/generations")
    } else if path.ends_with(EDITS) {
        Some("/images/edits")
    } else {
        None
    }
}

/// `resolveGPTImage2BaseModel`.
fn main_model(configured: &str) -> &str {
    if !configured.is_empty() && configured.go_lower().starts_with("gpt-") {
        configured
    } else {
        MAIN_MODEL
    }
}

/// `codexPrepareDirectOpenAIImageEditPayload` for a multipart edit: the form is rewritten
/// to JSON with data URLs. A JSON edit takes [`direct_json_payload`], as a generation does.
fn direct_edit_form(
    payload: &[u8],
    model: &str,
    content_type: &str,
    stream: bool,
) -> Result<(Bytes, String), ExecError> {
    let unsupported = || plain_err(format!("unsupported OpenAI image edit Content-Type {content_type:?}"));
    let MediaType::Ok(media, params) = multipart::parse_media_type(content_type.trim()) else {
        return Err(unsupported());
    };
    if !media.trim().starts_with("multipart/") {
        return Err(unsupported());
    }
    let boundary = params.get("boundary").map(|b| b.trim()).unwrap_or_default();
    if boundary.is_empty() {
        return Err(plain_err("multipart boundary is missing"));
    }
    let form =
        multipart::read_form(payload, boundary).map_err(|e| plain_err(format!("read multipart form failed: {e}")))?;
    Ok((
        Bytes::from(edit_form_to_json(&form, model, stream)?),
        "application/json".into(),
    ))
}

/// `codexRewriteOpenAIImageEditMultipartToJSON`.
// ponytail: Go ranges over its form map, so field order varies per run; this keeps the
// order fields arrived in, one of Go's orders.
fn edit_form_to_json(form: &Form, model: &str, stream: bool) -> Result<String, ExecError> {
    // The routed `model` and `stream` replace the form's, but the form's are still read
    // under the Codex rules first rather than skipped (#22).
    form_routing(form)?;
    let mut out = set_str("{}", "model", model);
    if stream {
        out = set_raw(&out, "stream", "true");
    }
    for (key, values) in &form.values {
        for value in values {
            form_utf8(key, value)?;
        }
        if key == "model" || key == "stream" || values.is_empty() {
            continue;
        }
        let path = match key.as_str() {
            "mask[file_id]" => Cow::Borrowed("mask.file_id"),
            "mask[image_url]" => Cow::Borrowed("mask.image_url"),
            other => {
                // A literal object key, not an editor path: preserve spaces and path
                // metacharacters. The prefix also permits an empty or numeric key.
                let mut path = String::from(":");
                for c in other.chars() {
                    if matches!(c, '.' | '\\' | '|' | '#' | '@' | '*' | '?') {
                        path.push('\\');
                    }
                    path.push(c);
                }
                Cow::Owned(path)
            }
        };
        let raw = if let [value] = values.as_slice() {
            form_json_value(key, value)?
        } else {
            let items = values
                .iter()
                .map(|v| form_json_value(key, v))
                .collect::<Result<Vec<_>, _>>()?;
            format!("[{}]", items.join(","))
        };
        out = set_raw(&out, &path, &raw);
    }
    if let Some(mask) = form.files("mask").first() {
        out = set_str(&out, "mask.image_url", &data_url(mask));
    }
    let files = image_files(form);
    let existing = rawjson::get(&out, "images");
    if !existing.exists() || existing.kind() == Kind::Array {
        let mut items: Vec<String> = existing.items().iter().map(|v| v.json().to_owned()).collect();
        for file in files {
            items.push(set_str(r#"{"image_url":""}"#, "image_url", &data_url(file)));
        }
        if !files.is_empty() {
            out = set_raw(&out, "images", &format!("[{}]", items.join(",")));
        }
    } else {
        for file in files {
            out = set_str(&out, "images.-1.image_url", &data_url(file));
        }
    }
    Ok(out)
}

/// `codexOpenAIImageEditFormJSONValue`: integers for the count fields, strings otherwise.
/// A value that is not UTF-8 is refused rather than replaced, and the count fields are
/// matched by their exact names; Go lowercases the field name with Unicode rules (#22).
/// This is the direct profile's form-to-JSON rule: a count whose text is a decimal
/// `i64` becomes that JSON integer, and any other count text (`nope`, `1.5`, an
/// overflowing integer) stays a JSON string, so the Images endpoint judges the value the
/// client sent. The tool path refuses such a count instead ([`edit_multipart`]): it has
/// no endpoint of the client's to judge it, and omitting it would mean the default.
fn form_json_value(key: &str, value: &[u8]) -> Result<String, ExecError> {
    let text = form_utf8(key, value)?.trim();
    if matches!(key, "n" | "output_compression" | "partial_images")
        && let Ok(n) = go_parse_int(text)
    {
        return Ok(n.to_string());
    }
    Ok(String::from_utf8_lossy(&cpa_common::json::quote(text)).into_owned())
}

/// A request error about multipart text field `key`. It names the field, never its value.
fn form_error(key: &str, problem: std::fmt::Arguments<'_>) -> ExecError {
    ExecError::local(
        400,
        cpa_core::exec::FailureScope::Request,
        format!("request form field `{key}` {problem}"),
    )
}

/// A multipart text field's value as UTF-8, or a request error that names the field.
fn form_utf8<'a>(key: &str, value: &'a [u8]) -> Result<&'a str, ExecError> {
    std::str::from_utf8(value)
        .map_err(|e| form_error(key, format_args!("is not valid UTF-8 at byte {}", e.valid_up_to())))
}

/// The one value of text field `key` as UTF-8, `None` when the form lacks it. A field
/// read for one meaning must not repeat.
fn single_text<'a>(form: &'a Form, key: &str) -> Result<Option<&'a str>, ExecError> {
    let Some((_, values)) = form.values.iter().find(|(k, _)| k == key) else {
        return Ok(None);
    };
    match values.as_slice() {
        [] => Ok(None),
        [value] => form_utf8(key, value).map(Some),
        _ => Err(form_error(key, format_args!("is repeated"))),
    }
}

/// The routing fields of a Codex multipart edit, which decide where the request goes and
/// whether it streams: `model` and `stream` appear at most once, as UTF-8, and `stream`
/// is a boolean. Returns the client's `stream`.
///
/// Form text carries no JSON type, so `stream` takes the spellings Go's `parseBoolField`
/// reads: `true`, `1`, `yes`, `on`, and `false`, `0`, `no`, `off`. They are protocol
/// tokens, matched with ASCII white space trimmed and ASCII case ignored. An empty value
/// is false, as an unset form field is. Any other value is refused; Go reads it as false.
///
/// The shared Images handler applies this to the client's form before it rebuilds the
/// form without these fields (`cpa_server::images`), and both adapter paths apply it to
/// the form they receive (#22).
pub fn form_routing(form: &Form) -> Result<bool, ExecError> {
    single_text(form, "model")?;
    let Some(text) = single_text(form, "stream")? else {
        return Ok(false);
    };
    let token = text.trim_ascii();
    let spelled = |words: &[&str]| words.iter().any(|w| token.eq_ignore_ascii_case(w));
    if spelled(&["true", "1", "yes", "on"]) {
        Ok(true)
    } else if spelled(&["", "false", "0", "no", "off"]) {
        Ok(false)
    } else {
        Err(form_error("stream", format_args!("must be a boolean")))
    }
}

/// The Codex adapter's rules for a client's Images JSON ([`ImagesJson::parse`]). The
/// shared Images handler applies them to the client's original bytes before it edits
/// `model` and `stream`, so that its edits cannot erase a fault the adapter would refuse,
/// such as a repeated `stream` (#22).
pub fn check_json(raw: &[u8]) -> Result<(), ExecError> {
    ImagesJson::parse(raw).map(|_| ())
}

/// Whether `text`, a number in JSON spelling, is exactly one. The decimal digits are
/// compared, never a float: `1`, `1.0`, `1e0`, `10e-1` and `0.1e1` are one, and
/// `1.0000000000000001`, which a float rounds to one, is not. Text that is not a JSON
/// number is not one. One pass over the spelling.
fn decimal_is_one(text: &str) -> bool {
    let b = text.as_bytes();
    let digits_from = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        i
    };
    // A negative number is never one; `-0` is zero.
    let int_end = digits_from(0);
    let int = &b[..int_end];
    if int.is_empty() || (int.len() > 1 && int[0] == b'0') {
        return false;
    }
    let mut i = int_end;
    let mut frac: &[u8] = &[];
    if b.get(i) == Some(&b'.') {
        let end = digits_from(i + 1);
        frac = &b[i + 1..end];
        if frac.is_empty() {
            return false;
        }
        i = end;
    }
    let mut exponent: i128 = 0;
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        let negative = b.get(i) == Some(&b'-');
        if matches!(b.get(i), Some(b'-' | b'+')) {
            i += 1;
        }
        let end = digits_from(i);
        // An exponent beyond `i64` cannot bring a body-sized mantissa back to one.
        let Ok(e) = text[i..end].parse::<i64>() else {
            return false;
        };
        exponent = if negative { -i128::from(e) } else { i128::from(e) };
        i = end;
    }
    if i != b.len() {
        return false;
    }
    // The value is the digit string times 10^(exponent - frac.len()). It is one when
    // exactly one digit is not zero, that digit is 1, and its place cancels the exponent.
    let mut one_at = None;
    for (at, &digit) in int.iter().chain(frac).enumerate() {
        match (digit, one_at) {
            (b'0', _) => {}
            (b'1', None) => one_at = Some(at),
            _ => return false,
        }
    }
    let Some(at) = one_at else {
        return false;
    };
    let (Ok(len), Ok(at), Ok(frac_len)) = (
        i128::try_from(int.len() + frac.len()),
        i128::try_from(at),
        i128::try_from(frac.len()),
    ) else {
        return false;
    };
    len - 1 - at + exponent - frac_len == 0
}

/// `strconv.ParseInt(s, 10, 64)`.
fn go_parse_int(s: &str) -> Result<i64, std::num::ParseIntError> {
    s.parse::<i64>()
}

/// `codexMultipartImageFiles`: `image[]` files, else `image` files.
fn image_files(form: &Form) -> &[Part] {
    match form.files("image[]") {
        [] => form.files("image"),
        files => files,
    }
}

/// `codexMultipartFileToDataURL`: the part's Content-Type, else the sniffed type.
fn data_url(part: &Part) -> String {
    // ponytail: a Content-Type that is not UTF-8 reaches the data URL lossily (image
    // clients send ASCII media types); building the URL as bytes would keep it.
    let header = String::from_utf8_lossy(part.header("Content-Type").unwrap_or_default());
    let media = Some(header.trim())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| detect_content_type(&part.body));
    format!(
        "data:{media};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&part.body)
    )
}

/// `http.DetectContentType` for uploaded images.
// ponytail: Go's image, BOM and text-versus-binary rules only; its HTML, XML, PDF, audio,
// video, archive and font signatures are not ported (image uploads do not carry them).
fn detect_content_type(data: &[u8]) -> &'static str {
    let data = &data[..data.len().min(512)];
    const EXACT: [(&[u8], &str); 8] = [
        (b"\xFE\xFF", "text/plain; charset=utf-16be"),
        (b"\xFF\xFE", "text/plain; charset=utf-16le"),
        (b"\xEF\xBB\xBF", "text/plain; charset=utf-8"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"\x89PNG\x0D\x0A\x1A\x0A", "image/png"),
        (b"\xFF\xD8\xFF", "image/jpeg"),
        (b"BM", "image/bmp"),
    ];
    for (signature, media) in EXACT {
        if data.starts_with(signature) {
            return media;
        }
    }
    if data.len() >= 14 && &data[..4] == b"RIFF" && &data[8..14] == b"WEBPVP" {
        return "image/webp";
    }
    if data.starts_with(b"\x00\x00\x01\x00") {
        return "image/x-icon";
    }
    let binary = |b: &u8| matches!(*b, 0x00..=0x08 | 0x0B | 0x0E..=0x1A | 0x1C..=0x1F);
    if data.iter().any(binary) {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
}

/// Whether the request is a multipart edit: an edit whose `Content-Type` names a
/// multipart media type. Every other Images body is JSON.
fn is_form(req: &ExecRequest, path: &str) -> bool {
    if !path.ends_with(EDITS) {
        return false;
    }
    let content_type = request::header(&req.headers, "content-type").trim();
    match multipart::parse_media_type(content_type) {
        MediaType::Ok(media, _) | MediaType::BadParams(media) => media.starts_with("multipart/"),
        MediaType::Invalid(_) => false,
    }
}

/// `codexPrepareOpenAIImageRequest`. `json` is the body read through the boundary; it is
/// `None` only for a multipart edit ([`is_form`]).
fn prepare(req: &ExecRequest, json: Option<&ImagesJson<'_>>, path: &str) -> Result<Prepared, ExecError> {
    if !path.ends_with(GENERATIONS) && !path.ends_with(EDITS) {
        return Err(plain_err(format!("unsupported OpenAI image endpoint path {path:?}")));
    }
    match json {
        Some(json) if path.ends_with(GENERATIONS) => generation_json(json, &req.model),
        Some(json) => edit_json(json, &req.model),
        None => edit_multipart(
            &req.body,
            &req.model,
            request::header(&req.headers, "content-type").trim(),
        ),
    }
}

/// A client's Images JSON request after the Codex request boundary
/// ([`cpa_common::codex_body`]): UTF-8, RFC 8259 JSON with nothing after it, no key
/// repeated in any object, at most 100 levels deep, at most 64 MiB, and an object (#22).
/// The members the adapter reads are typed where they are read. A member of the wrong
/// type is refused; it is never read as text or as a number, as Go's gjson reads it.
/// `null` reads as absent, as the Images API documents for its optional parameters.
struct ImagesJson<'a>(CodexBody<'a>);

impl<'a> ImagesJson<'a> {
    fn parse(raw: &'a [u8]) -> Result<Self, ExecError> {
        let json = Self(CodexBody::parse_object(raw)?);
        json.string("model")?;
        match json.0.get("stream").map(|m| m.kind) {
            None | Some(body::Kind::True | body::Kind::False | body::Kind::Null) => {}
            Some(_) => {
                return Err(BodyError::FieldType {
                    field: "stream",
                    expected: "a boolean",
                }
                .into());
            }
        }
        Ok(json)
    }

    /// The client's `model`, which [`Self::parse`] checked.
    fn model(&self) -> Option<Cow<'a, str>> {
        self.string("model").ok().flatten()
    }

    fn string(&self, field: &'static str) -> Result<Option<Cow<'a, str>>, ExecError> {
        self.0.get(field).map_or(Ok(None), |m| typed_string(m, field))
    }

    /// A number member's exact spelling: it is copied, never converted.
    fn number(&self, field: &'static str) -> Result<Option<&'a str>, ExecError> {
        match self.0.get(field) {
            None => Ok(None),
            Some(m) => match m.kind {
                body::Kind::Null => Ok(None),
                // The scanner checked the number grammar, which is ASCII.
                body::Kind::Number => Ok(std::str::from_utf8(m.raw).ok()),
                _ => Err(BodyError::FieldType {
                    field,
                    expected: "a number",
                }
                .into()),
            },
        }
    }
}

fn typed_string<'a>(member: &Member<'a>, field: &'static str) -> Result<Option<Cow<'a, str>>, ExecError> {
    match member.kind {
        body::Kind::Null => Ok(None),
        body::Kind::String => Ok(member.text()),
        _ => Err(BodyError::FieldType {
            field,
            expected: "a string",
        }
        .into()),
    }
}

/// Images API fields the `image_generation` tool expresses (OpenAI Images API and the
/// Responses image generation tool). Any other field, or `n` above 1, is refused before
/// anything is sent (#27); Go drops them.
const TOOL_FIELDS: [&str; 11] = [
    "model",
    "prompt",
    "size",
    "quality",
    "background",
    "output_format",
    "output_compression",
    "partial_images",
    "moderation",
    "response_format",
    "stream",
];

fn unexpressible(field: &str) -> ExecError {
    ExecError::local(
        400,
        cpa_core::exec::FailureScope::Request,
        format!(
            "unsupported conversion: `{field}` cannot be expressed by the Codex image generation tool; remove it or send the request to a provider that accepts it"
        ),
    )
}

/// Checks each field of a request: a [`TOOL_FIELDS`] or `extra` field, or `n` when the
/// request's `n` is one image; anything else is refused.
fn check_fields<'a>(
    fields: impl IntoIterator<Item = &'a str>,
    n_is_one: bool,
    extra: &[&str],
) -> Result<(), ExecError> {
    for field in fields {
        let one = field == "n" && n_is_one;
        if !(TOOL_FIELDS.contains(&field) || extra.contains(&field) || one) {
            return Err(unexpressible(field));
        }
    }
    Ok(())
}

fn expressible(json: &ImagesJson<'_>, extra: &[&str]) -> Result<(), ExecError> {
    // `n` is a number, compared with one exactly ([`decimal_is_one`]).
    let n_is_one = json.number("n")?.is_none_or(decimal_is_one);
    check_fields(json.0.members().iter().map(|m| &*m.key), n_is_one, extra)
}

/// An image the tool request carries: a URL (a data URL included) or an uploaded file.
enum Image {
    Url(String),
    File(String),
}

/// A JSON image reference (`{"image_url":...}` or `{"file_id":...}`, the Images API
/// forms the Responses `input_image` and `input_image_mask` also take). Anything else is
/// refused rather than dropped. `raw` is part of a body that passed the boundary.
fn image_reference(raw: &[u8], what: &str) -> Result<Image, ExecError> {
    let refused = || unexpressible(&format!("{what} without exactly one image_url or file_id"));
    let Ok(object) = CodexBody::parse_object(raw) else {
        return Err(refused());
    };
    let text = |field: &'static str| -> Result<String, ExecError> {
        let value = object.get(field).map_or(Ok(None), |m| typed_string(m, field))?;
        Ok(value.as_deref().unwrap_or_default().trim().to_owned())
    };
    let (url, file) = (text("image_url")?, text("file_id")?);
    match (url.is_empty(), file.is_empty()) {
        (false, true) => Ok(Image::Url(url)),
        (true, false) => Ok(Image::File(file)),
        _ => Err(refused()),
    }
}

/// `codexPrepareOpenAIImageGenerationJSON`.
fn generation_json(json: &ImagesJson<'_>, route_model: &str) -> Result<Prepared, ExecError> {
    expressible(json, &[])?;
    let prompt = json.string("prompt")?.unwrap_or_default().trim().to_owned();
    let tool = tool_json(
        json,
        route_model,
        "generate",
        &["size", "quality", "background", "output_format", "moderation"],
    )?;
    Ok(Prepared {
        body: responses_request(&prompt, &[], &tool),
        url_format: url_format(&json.string("response_format")?.unwrap_or_default()),
        prefix: "image_generation",
    })
}

/// `codexPrepareOpenAIImageEditJSON`.
fn edit_json(json: &ImagesJson<'_>, route_model: &str) -> Result<Prepared, ExecError> {
    expressible(json, &["images", "mask", "input_fidelity"])?;
    let prompt = json.string("prompt")?.unwrap_or_default().trim().to_owned();
    let images: Vec<Image> = match json.0.get("images") {
        None => Vec::new(),
        Some(images) => match images.items() {
            Some(items) => items
                .into_iter()
                .map(|item| image_reference(item, "an image"))
                .collect::<Result<_, _>>()?,
            None => return Err(unexpressible("images that is not an array")),
        },
    };
    let mut tool = tool_json(
        json,
        route_model,
        "edit",
        &[
            "size",
            "quality",
            "background",
            "output_format",
            "input_fidelity",
            "moderation",
        ],
    )?;
    if let Some(mask) = json.0.get("mask").filter(|m| m.kind != body::Kind::Null) {
        tool = match image_reference(mask.raw, "a mask")? {
            Image::Url(url) => set_str(&tool, "input_image_mask.image_url", &url),
            Image::File(id) => set_str(&tool, "input_image_mask.file_id", &id),
        };
    }
    Ok(Prepared {
        body: responses_request(&prompt, &images, &tool),
        url_format: url_format(&json.string("response_format")?.unwrap_or_default()),
        prefix: "image_edit",
    })
}

/// `codexPrepareOpenAIImageEditMultipart`.
fn edit_multipart(body: &[u8], route_model: &str, content_type: &str) -> Result<Prepared, ExecError> {
    let params = match multipart::parse_media_type(content_type) {
        MediaType::Ok(_, params) => params,
        _ => {
            return Err(plain_err(
                "parse multipart content type failed: mime: invalid media parameter",
            ));
        }
    };
    let boundary = params.get("boundary").map(|b| b.trim()).unwrap_or_default();
    if boundary.is_empty() {
        return Err(plain_err("multipart boundary is required"));
    }
    let form =
        multipart::read_form(body, boundary).map_err(|e| plain_err(format!("parse multipart form failed: {e}")))?;
    // The tool reads one value per text field: a repeated field is ambiguous, and a
    // value that is not UTF-8 is refused rather than replaced (#22).
    for (key, values) in &form.values {
        if values.len() > 1 {
            return Err(ExecError::local(
                400,
                cpa_core::exec::FailureScope::Request,
                format!("request form field `{key}` is repeated"),
            ));
        }
        for value in values {
            form_utf8(key, value)?;
        }
    }
    // `stream` is read as the handler reads it, though `req.stream` decides the mode.
    form_routing(&form)?;
    let value = |key: &str| form.value(key).trim().to_owned();
    // The same field policy as JSON (#27): form values the tool expresses, and image and
    // mask files; anything else is refused. `n` is compared with one as its JSON
    // spelling, after an optional `+` that form text may carry.
    let n_is_one = single_text(&form, "n")?.is_none_or(|n| {
        let n = n.trim();
        decimal_is_one(n.strip_prefix('+').unwrap_or(n))
    });
    check_fields(
        form.values.iter().map(|(key, _)| key.as_str()),
        n_is_one,
        &["input_fidelity"],
    )?;
    if let Some((file, _)) = form
        .files
        .iter()
        .find(|(key, _)| !matches!(key.as_str(), "image" | "image[]" | "mask"))
    {
        return Err(unexpressible(&format!("file field {file}")));
    }
    if !form.files("image").is_empty() && !form.files("image[]").is_empty() {
        // Only one of the two image fields would be read.
        return Err(unexpressible("both image and image[] files"));
    }
    if form.files("mask").len() > 1 {
        return Err(unexpressible("more than one mask file"));
    }
    let mut tool = r#"{"type":"image_generation","action":"edit"}"#.to_owned();
    tool = set_str(&tool, "model", &tool_model(&value("model"), route_model));
    for field in [
        "size",
        "quality",
        "background",
        "output_format",
        "input_fidelity",
        "moderation",
    ] {
        let v = value(field);
        if !v.is_empty() {
            tool = set_str(&tool, field, &v);
        }
    }
    // A present count is a decimal `i64` (an optional sign, digits) or it is refused: the
    // tool request has no other spelling for it, and leaving it out would ask for the
    // provider's default instead of the client's value. Go drops it (#22).
    for field in ["output_compression", "partial_images"] {
        if let Some(text) = single_text(&form, field)? {
            let n = go_parse_int(text.trim()).map_err(|_| form_error(field, format_args!("must be an integer")))?;
            tool = set_raw(&tool, field, &n.to_string());
        }
    }
    let images: Vec<Image> = image_files(&form).iter().map(|f| Image::Url(data_url(f))).collect();
    if let Some(mask) = form.files("mask").first() {
        tool = set_str(&tool, "input_image_mask.image_url", &data_url(mask));
    }
    Ok(Prepared {
        body: responses_request(&value("prompt"), &images, &tool),
        url_format: url_format(&value("response_format")),
        prefix: "image_edit",
    })
}

/// `codexNormalizeImageResponseFormat`: `url`, else `b64_json`.
fn url_format(format: &str) -> bool {
    format.trim().eq_ignore_ascii_case("url")
}

/// `codexOpenAIImageToolModel`.
fn tool_model(request_model: &str, route_model: &str) -> String {
    [request_model.trim(), route_model.trim()]
        .into_iter()
        .find(|m| !m.is_empty())
        .unwrap_or(DEFAULT_TOOL_MODEL)
        .to_owned()
}

/// `codexBuildOpenAIImageTool`: the client's string fields, trimmed, and the count fields
/// with their number spelling. A field of another type is refused; Go reads it as text
/// with `String()`, and truncates or drops a count (#22).
fn tool_json(
    json: &ImagesJson<'_>,
    route_model: &str,
    action: &str,
    string_fields: &[&'static str],
) -> Result<String, ExecError> {
    let mut tool = set_str(r#"{"type":"image_generation","action":""}"#, "action", action);
    let model = json.model().unwrap_or_default();
    tool = set_str(&tool, "model", &tool_model(&model, route_model));
    for &field in string_fields {
        if let Some(value) = json.string(field)?
            && !value.trim().is_empty()
        {
            tool = set_str(&tool, field, value.trim());
        }
    }
    for field in ["output_compression", "partial_images"] {
        if let Some(raw) = json.number(field)? {
            tool = set_raw(&tool, field, raw);
        }
    }
    Ok(tool)
}

/// `codexBuildImagesResponsesRequest`.
fn responses_request(prompt: &str, images: &[Image], tool: &str) -> String {
    // Only what the conversion needs (#27): the model, the forced `image_generation`
    // tool with the client's image settings, the prompt and images, `stream` (the answer
    // is read from events) and `store: false` as the pinned client sends. No default
    // instructions, reasoning, `parallel_tool_calls` or `include` (Go adds them).
    let mut req =
        r#"{"stream":true,"model":"","store":false,"tool_choice":{"type":"image_generation"},"tools":[]}"#.to_owned();
    req = set_str(&req, "model", MAIN_MODEL);
    if !tool.is_empty() && rawjson::valid(tool) {
        req = set_raw(&req, "tools", &format!("[{tool}]"));
    }
    let mut content = vec![set_str(r#"{"type":"input_text","text":""}"#, "text", prompt)];
    for image in images {
        content.push(match image {
            Image::Url(url) => set_str(r#"{"type":"input_image","image_url":""}"#, "image_url", url),
            Image::File(id) => set_str(r#"{"type":"input_image","file_id":""}"#, "file_id", id),
        });
    }
    let input = format!(
        r#"[{{"type":"message","role":"user","content":[{}]}}]"#,
        content.join(",")
    );
    set_raw(&req, "input", &input)
}

/// `prepareCodexOpenAIImageBody`: thinking, then the Codex request rules.
fn tool_body(body: &str, main_model: &str) -> Result<String, ExecError> {
    let thought = cpa_common::thinking::apply_thinking_with_source_payload(
        body.as_bytes(),
        body.as_bytes(),
        body.as_bytes(),
        main_model,
        SOURCE,
        "codex",
        "codex",
        false,
    )
    .map_err(|e| ExecError::local(e.status(), cpa_core::exec::FailureScope::Request, e.message))?;
    let mut out = String::from_utf8_lossy(&thought).into_owned();
    out = set_str_if_different(out, "model", main_model);
    out = set_bool_if_different(out, "stream", true);
    for key in [
        "previous_response_id",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
    ] {
        out = delete(&out, key);
    }
    Ok(out)
}

/// `codexExtractImageResults`: image calls from the completed output, else from the
/// collected `output_item.done` items in index order; the creation time (now when
/// absent); and `tool_usage.image_gen`.
fn extract(completed: &str, items: &OutputItems) -> (Vec<ImageCall>, i64, Option<String>) {
    let mut created = rawjson::get(completed, "response.created_at").i64();
    if created <= 0 {
        created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
    }
    let patched = items.patch(completed.to_owned());
    let calls = rawjson::get(&patched, "response.output")
        .items()
        .iter()
        .filter(|item| item.get("type").str() == "image_generation_call")
        .filter_map(|item| {
            let field = |k: &str| item.get(k).str().trim().to_owned();
            let result = field("result");
            (!result.is_empty()).then(|| ImageCall {
                result,
                revised_prompt: field("revised_prompt"),
                output_format: field("output_format"),
                size: field("size"),
                background: field("background"),
                quality: field("quality"),
            })
        })
        .collect();
    let usage = rawjson::get(completed, "response.tool_usage.image_gen");
    let usage = (usage.kind() == Kind::Object).then(|| usage.json().to_owned());
    (calls, created, usage)
}

/// `codexBuildImagesAPIResponse`.
fn images_response(calls: &[ImageCall], created: i64, usage: Option<&str>, url: bool) -> String {
    let mut out = set_raw(r#"{"created":0,"data":[]}"#, "created", &created.to_string());
    let first = calls.first().cloned().unwrap_or_default();
    for (key, value) in [
        ("background", &first.background),
        ("output_format", &first.output_format),
        ("quality", &first.quality),
        ("size", &first.size),
    ] {
        if !value.is_empty() {
            out = set_str(&out, key, value);
        }
    }
    if let Some(usage) = usage.filter(|u| rawjson::valid(u)) {
        out = set_raw(&out, "usage", usage);
    }
    let items: Vec<String> = calls
        .iter()
        .map(|call| {
            let mut item = "{}".to_owned();
            if !call.revised_prompt.is_empty() {
                item = set_str(&item, "revised_prompt", &call.revised_prompt);
            }
            image_field(item, call, url)
        })
        .collect();
    set_raw(&out, "data", &format!("[{}]", items.join(",")))
}

/// `url` as a data URL, else `b64_json`.
fn image_field(json: String, call: &ImageCall, url: bool) -> String {
    if url {
        set_str(
            &json,
            "url",
            &format!("data:{};base64,{}", mime_type(&call.output_format), call.result),
        )
    } else {
        set_str(&json, "b64_json", &call.result)
    }
}

/// `codexBuildImagePartialFrame`.
fn partial_frame(payload: &str, prepared: &Prepared) -> Option<String> {
    let b64 = rawjson::get(payload, "partial_image_b64").str().trim().to_owned();
    if b64.is_empty() {
        return None;
    }
    let call = ImageCall {
        result: b64,
        output_format: rawjson::get(payload, "output_format").str().trim().to_owned(),
        ..Default::default()
    };
    let event = format!("{}.partial_image", prepared.prefix);
    let mut data = set_str(r#"{"type":"","partial_image_index":0}"#, "type", &event);
    data = set_raw(
        &data,
        "partial_image_index",
        &rawjson::get(payload, "partial_image_index").i64().to_string(),
    );
    Some(sse_frame(&event, &image_field(data, &call, prepared.url_format)))
}

/// `codexBuildImageCompletedFrame`.
fn completed_frame(call: &ImageCall, usage: Option<&str>, prepared: &Prepared) -> String {
    let event = format!("{}.completed", prepared.prefix);
    let mut data = set_str(r#"{"type":""}"#, "type", &event);
    if let Some(usage) = usage.filter(|u| rawjson::valid(u)) {
        data = set_raw(&data, "usage", usage);
    }
    sse_frame(&event, &image_field(data, call, prepared.url_format))
}

fn sse_frame(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// `codexMimeTypeFromOutputFormat`.
fn mime_type(output_format: &str) -> &'static str {
    match output_format.trim().go_lower().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

/// `prepareOpenAICompatImagesPayload` for a JSON body that passed the boundary: the
/// routed `model`, and `stream` set or removed. The edits are sjson's, so a member the
/// body lacks is appended as Go appends it, and every other byte stays. `raw` holds the
/// bytes `json` read; they are shared, not copied, when nothing changes.
fn direct_json_payload(raw: &Bytes, json: &ImagesJson<'_>, model: &str, stream: bool) -> Bytes {
    let model_kept = model.is_empty() || json.model().as_deref() == Some(model);
    let stream_kept = match json.0.get("stream").map(|m| m.kind) {
        Some(body::Kind::True) => stream,
        None => !stream,
        Some(_) => false,
    };
    if model_kept && stream_kept {
        return raw.clone();
    }
    let mut body = json.0.raw().to_vec();
    if !model.is_empty() {
        crate::openai_payload::set_str_if_different(&mut body, "model", model);
    }
    if stream {
        crate::openai_payload::set_bool_if_different(&mut body, "stream", true);
    } else {
        cpa_common::json::delete(&mut body, "stream");
    }
    Bytes::from(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(requested: &str, resolved: &str, body: &'static [u8]) -> ExecRequest {
        ExecRequest {
            operation: cpa_core::exec::Operation::Generate,
            source_format: Format::OpenAI,
            response_format: Format::OpenAI,
            requested_model: requested.into(),
            model: resolved.into(),
            original_body: Bytes::from_static(body),
            body: Bytes::from_static(body),
            stream: false,
            alt: None,
            session: None,
            headers: Default::default(),
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: Default::default(),
            request_path: String::new(),
            caller: cpa_core::exec::Caller {
                principal: String::new(),
                source: "",
                provider: String::new(),
            },
        }
    }

    /// `n` is one image only when its decimal value is exactly one (#22). Spellings
    /// written by hand; a float comparison accepts `1.0000000000000001`.
    #[test]
    fn one_image_is_compared_by_decimal_digits() {
        for one in [
            "1", "1.0", "1.000", "1e0", "1E0", "1e+0", "1e-0", "10e-1", "0.1e1", "0.01e2", "100E-2", "1e00000",
        ] {
            assert!(decimal_is_one(one), "{one}");
        }
        for other in [
            "1.0000000000000001",
            "0.9999999999999999999",
            "2",
            "11",
            "0",
            "-1",
            "-0",
            "0.0",
            "1e1",
            "1e-1",
            "10",
            "0.1",
            "1e99999999999999999999",
            "1e-99999999999999999999",
            "01",
            "+1",
            "1.",
            ".1e1",
            "1e",
            "1e+",
            "1 ",
            "",
            "one",
            "inf",
            "NaN",
        ] {
            assert!(!decimal_is_one(other), "{other}");
        }
    }

    fn form_of(fields: &[(&str, &[u8])]) -> Form {
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!("--b\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(value);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"--b--\r\n");
        multipart::read_form(&body, "b").unwrap()
    }

    /// The routing fields of a Codex form are read once, as UTF-8, and `stream` as one
    /// of the boolean spellings; nothing else becomes false.
    #[test]
    fn form_routing_reads_model_and_stream_once() {
        for (text, want) in [
            (&b"true"[..], true),
            (b"TRUE", true),
            (b" 1 ", true),
            (b"yes", true),
            (b"On", true),
            (b"false", false),
            (b"0", false),
            (b"no", false),
            (b"off", false),
            (b"", false),
        ] {
            let form = form_of(&[("model", b"gpt-image-2"), ("stream", text)]);
            assert_eq!(form_routing(&form).unwrap(), want, "{text:?}");
        }
        assert!(!form_routing(&form_of(&[("prompt", b"p")])).unwrap());
        for (fields, message) in [
            (
                vec![("stream", &b"nope"[..])],
                "request form field `stream` must be a boolean",
            ),
            (
                vec![("stream", &b"\xc2\xa0true"[..])],
                "request form field `stream` must be a boolean",
            ),
            (
                vec![("stream", &b"\xff"[..])],
                "request form field `stream` is not valid UTF-8 at byte 0",
            ),
            (
                vec![("stream", &b"false"[..]), ("stream", &b"true"[..])],
                "request form field `stream` is repeated",
            ),
            (
                vec![("model", &b"gpt-image-2"[..]), ("model", &b"gpt-image-1.5"[..])],
                "request form field `model` is repeated",
            ),
            (
                vec![("model", &b"\xff/gpt-image-2"[..])],
                "request form field `model` is not valid UTF-8 at byte 0",
            ),
        ] {
            let error = form_routing(&form_of(&fields)).unwrap_err();
            assert_eq!(
                (error.status, error.scope),
                (400, cpa_core::exec::FailureScope::Request)
            );
            assert_eq!(String::from_utf8_lossy(&error.body), message, "{fields:?}");
        }
    }

    /// [`direct_model`] with the payload model read as `images` reads it: from a JSON
    /// body, and none from a form.
    fn route(req: &ExecRequest, settings: &Settings) -> Option<String> {
        let json = ImagesJson::parse(&req.body).ok();
        direct_model(req, json.as_ref().and_then(ImagesJson::model).as_deref(), settings)
    }

    /// Go `TestCodexImagePayloadPreservesResolvedAlias` and the model column of
    /// `TestCodexImageResolvedModelOnWire` (its payload-rule rows need the media payload
    /// finalizer, 868e137): a configured upstream name is sent as it is, and only an
    /// unconfigured gpt-image-* route loses its prefix and case.
    #[test]
    fn direct_images_keep_the_resolved_upstream_model() {
        let plain = Settings::default();
        let json: &[u8] = br#"{"model":"gpt-image-2","prompt":"diagnostic"}"#;
        let form: &[u8] = b"--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nx\r\n--b--\r\n";
        assert_eq!(
            route(&request("", "openai/gpt-image-2-cpr", json), &plain).as_deref(),
            Some("openai/gpt-image-2-cpr")
        );
        for (requested, resolved, configured, want) in [
            (
                "gpt-image-2",
                "openai/gpt-image-2-cpr",
                "openai/gpt-image-2-cpr",
                "openai/gpt-image-2-cpr",
            ),
            (
                "gpt-image-2",
                "openai/gpt-image-2",
                "openai/gpt-image-2",
                "openai/gpt-image-2",
            ),
            (
                "gpt-image-2",
                "OpenAI/GPT-Image-2",
                "OpenAI/GPT-Image-2",
                "OpenAI/GPT-Image-2",
            ),
            ("gpt-image-2", "gpt-image-1.5", "gpt-image-1.5", "gpt-image-1.5"),
            (
                "gpt-image-2",
                "codex/gpt-image-2",
                "codex/gpt-image-2",
                "codex/gpt-image-2",
            ),
            (
                "openai/gpt-image-2",
                "openai/gpt-image-2",
                "openai/gpt-image-2",
                "openai/gpt-image-2",
            ),
            (
                "tenant/gpt-image-2(high)",
                "OpenAI/GPT-Image-2(high)",
                "OpenAI/GPT-Image-2",
                "OpenAI/GPT-Image-2",
            ),
            (
                "codex/gpt-image-1.5(high)",
                "codex/gpt-image-1.5(high)",
                "",
                "gpt-image-1.5",
            ),
            ("CODEX/GPT-Image-2(high)", "CODEX/GPT-Image-2(high)", "", "gpt-image-2"),
        ] {
            let settings = Settings {
                key_models: if configured.is_empty() {
                    Vec::new()
                } else {
                    vec![serde_json::json!({"name": configured, "alias": "gpt-image-2"})]
                },
                ..Settings::default()
            };
            // A multipart edit has no JSON model; the client's model carries the route.
            for body in [json, form] {
                assert_eq!(
                    route(&request(requested, resolved, body), &settings).as_deref(),
                    Some(want),
                    "{requested} -> {resolved}"
                );
            }
        }
    }

    /// Go `TestCodexExecutorDirectOpenAIImage25Models`, with the GPT Image 2.6 rows of
    /// CLIProxyAPI PR #7261 (docs/DIFFERENCES-FROM-GO.md): each model, prefixed or not,
    /// takes the direct Images route and goes upstream without its prefix and suffix. The
    /// bare `gpt-image-2.6` is no direct model and keeps the Responses image tool.
    #[test]
    fn gpt_image_25_and_26_are_direct_models() {
        let plain = Settings::default();
        let form: &[u8] = b"--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nx\r\n--b--\r\n";
        for base in [
            "gpt-image-2.5",
            "gpt-image-2.5-flare",
            "gpt-image-2.5-sunburst",
            "gpt-image-2.6-flare",
            "gpt-image-2.6-sunburst",
        ] {
            for model in [
                base.to_owned(),
                format!("codex/{base}"),
                format!("codex/{base}(high)"),
                base.go_upper(),
            ] {
                // The request helper borrows a static body.
                let json: &'static [u8] = serde_json::json!({"model": model, "prompt": "x"})
                    .to_string()
                    .into_bytes()
                    .leak();
                for body in [json, form] {
                    assert_eq!(
                        route(&request(&model, &model, body), &plain).as_deref(),
                        Some(base),
                        "{model}"
                    );
                }
            }
        }
        let json: &[u8] = br#"{"model":"gpt-image-2.6","prompt":"x"}"#;
        assert_eq!(route(&request("gpt-image-2.6", "gpt-image-2.6", json), &plain), None);
    }
}
