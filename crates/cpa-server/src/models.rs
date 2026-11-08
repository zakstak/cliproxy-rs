//! Model listing routes: unified `GET /v1/models` and the Gemini catalog
//! (internal/api/server_routes.go, internal/registry/model_registry.go).

use std::sync::Arc;

use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use cpa_common::gostr::GoStr;
use serde_json::{Map, Value, json};

use crate::registry::Spec;
use crate::{Runtime, gojson, respond};

const CLAUDE_MAX_INPUT: i64 = 200_000;
const CLAUDE_MAX_OUTPUT: i64 = 64_000;

/// `GET /v1/models`: Grok Shell (a `grok-shell` User-Agent) gets its own catalog, Codex
/// clients (any `client_version` query key) the Codex client catalog, Anthropic clients
/// (an `Anthropic-Version` header or a `claude-cli` User-Agent) the Anthropic catalog, and
/// everyone else the OpenAI one.
pub async fn unified(State(rt): State<Arc<Runtime>>, uri: OriginalUri, headers: HeaderMap) -> Response {
    list(rt, uri, headers, None).await
}

async fn list(rt: Arc<Runtime>, OriginalUri(uri): OriginalUri, headers: HeaderMap, detail: Option<&str>) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
    };
    let anthropic = header("anthropic-version").is_some_and(|v| !v.is_empty())
        || header("user-agent").is_some_and(|ua| ua.starts_with("claude-cli"));
    let disable_cloaking = || {
        rt.config()
            .document
            .get("upstream")
            .and_then(|u| u.get("claude"))
            .and_then(|c| c.get("disable-cloaking-model-list"))
            .and_then(serde_yaml_ng::Value::as_bool)
            .unwrap_or(false)
    };
    // Go `WriteModelListResponse`; with `detail`, that model's entry of the list.
    let intercept = |_source: &'static str, response: Response| async move { with_detail(response, detail).await };
    // Go Home mode: the catalog Home answers for this client.
    if let Some(remote) = rt.remote_dispatch() {
        let entries = match crate::home_models::load(remote.as_ref(), &headers, &uri).await {
            Ok(entries) => entries,
            Err(response) => return *response,
        };
        let grok_shell = header("user-agent").is_some_and(|ua| ua.go_lower().contains("grok-shell"));
        // ponytail: Go builds the Codex client catalog (`client_version`) from Home's
        // model IDs with its template metadata; here such clients get the OpenAI list.
        let (source, body) = if grok_shell {
            ("openai", crate::home_models::grok(&entries))
        } else if anthropic && query_value(&uri, "client_version").is_none() {
            ("claude", crate::home_models::claude(&entries, disable_cloaking()))
        } else {
            ("openai", crate::home_models::openai(&entries))
        };
        return intercept(source, respond::gin_json(200, body)).await;
    }
    if header("user-agent").is_some_and(|ua| ua.go_lower().contains("grok-shell")) {
        let registry = rt.registry();
        let list = grok_list(registry.available_with(|c, m| rt.suspension(c, m)));
        return intercept("openai", respond::gin_json(200, list)).await;
    }
    if let Some(version) = query_value(&uri, "client_version") {
        return intercept("openai", crate::codex_models::response(&rt, &version)).await;
    }
    let registry = rt.registry();
    let (source, body) = if anthropic {
        (
            "claude",
            claude_list(registry.available_with(|c, m| rt.suspension(c, m)), disable_cloaking()),
        )
    } else {
        (
            "openai",
            openai_list(registry.available_with(|c, m| rt.suspension(c, m))),
        )
    };
    intercept(source, respond::gin_json(200, body)).await
}

/// The answer for `detail`, a model detail lookup (Go `ModelDetailIDContextKey`), in a
/// built 200 model list: that model's entry; the list as is without one.
async fn with_detail(response: Response, detail: Option<&str>) -> Response {
    let Some(id) = detail.filter(|_| response.status() == axum::http::StatusCode::OK) else {
        return response;
    };
    let (mut parts, body) = response.into_parts();
    let Ok(body) = axum::body::to_bytes(body, usize::MAX).await else {
        return Response::from_parts(parts, axum::body::Body::empty());
    };
    let (status, body) = match select_detail(&body, id) {
        Ok(Some(entry)) => (200, body.slice_ref(entry)),
        Ok(None) => (
            404,
            axum::body::Bytes::from_static(
                br#"{"error":{"code":"model_not_found","message":"Model not found","type":"invalid_request_error"}}"#,
            ),
        ),
        Err(()) => (
            502,
            axum::body::Bytes::from_static(br#"{"error":{"message":"Invalid model catalog","type":"api_error"}}"#),
        ),
    };
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.status = axum::http::StatusCode::from_u16(status).unwrap_or_default();
    Response::from_parts(parts, axum::body::Body::from(body))
}

/// `GET /v1/models/*model`: the entry for `model` from the catalog `GET /v1/models` would
/// answer this client (Go `ModelDetailIDContextKey`).
pub async fn detail(
    State(rt): State<Arc<Runtime>>,
    model: Option<Path<String>>,
    uri: OriginalUri,
    headers: HeaderMap,
) -> Response {
    let id = model.map(|Path(model)| model).unwrap_or_default();
    list(rt, uri, headers, Some(&id)).await
}

/// Go `WriteModelListResponse` with a detail ID: the first entry of `data`, then
/// `models`, whose `id` (or `slug` when the ID is empty) equals `id`, as its raw bytes.
/// `Err` when the catalog does not decode as Go's `{data, models []json.RawMessage}`.
pub(crate) fn select_detail<'a>(body: &'a [u8], id: &str) -> Result<Option<&'a [u8]>, ()> {
    use cpa_common::json::{self, Kind};
    // encoding/json: a key matches a field case-insensitively, each match is decoded in
    // document order (so the last one wins), and one mismatched type fails the decode.
    fn fields<'a>(object: &'a [u8], names: [&str; 2], mut f: impl FnMut(usize, &'a [u8])) {
        json::object_members(object, |key, _, value| {
            if let Some(key) = json::go_unquote(&[b"\"", key, b"\""].concat()) {
                (0..2).filter(|&i| key.go_eq_fold(names[i])).for_each(|i| f(i, value));
            }
            true
        });
    }
    if !json::std_valid(body) {
        return Err(());
    }
    let top = json::parse(body);
    if top.kind == Kind::Null {
        return Ok(None);
    }
    if !top.is_object() {
        return Err(());
    }
    let (mut lists, mut mismatched) = ([None, None], false);
    fields(body, ["data", "models"], |i, value| match json::parse(value) {
        items if items.is_array() => lists[i] = Some(items),
        items if items.kind == Kind::Null => lists[i] = None,
        _ => mismatched = true,
    });
    if mismatched {
        return Err(());
    }
    for item in lists.iter().flatten().flat_map(|list| list.array()) {
        let std::borrow::Cow::Borrowed(raw) = item.raw else {
            continue;
        };
        if !item.is_object() {
            continue;
        }
        // A string sets the field and null leaves it alone; anything else fails the entry.
        let (mut found, mut mismatched) = ([String::new(), String::new()], false);
        fields(raw, ["id", "slug"], |i, value| match json::go_unquote(value) {
            Some(text) => found[i] = text,
            None => mismatched |= value != b"null",
        });
        let [entry_id, slug] = found;
        let entry_id = if entry_id.is_empty() { slug } else { entry_id };
        if !mismatched && !entry_id.is_empty() && entry_id == id {
            return Ok(Some(raw));
        }
    }
    Ok(None)
}

/// Go `c.Request.URL.Query()[key]` present, with `c.Query(key)`'s first value.
fn query_value(uri: &axum::http::Uri, key: &str) -> Option<String> {
    let axum::extract::Query(pairs) = axum::extract::Query::<Vec<(String, String)>>::try_from_uri(uri).ok()?;
    pairs.into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Go `OpenAIModels`: id, object, created and owned_by only.
pub fn openai_list<'a>(models: impl Iterator<Item = &'a Spec>) -> String {
    let data: Vec<Value> = models
        .map(|m| {
            let mut entry = Map::new();
            entry.insert("id".into(), m.id.clone().into());
            entry.insert("object".into(), "model".into());
            entry.insert("owned_by".into(), m.owned_by.clone().into());
            if m.created > 0 {
                entry.insert("created".into(), m.created.into());
            }
            Value::Object(entry)
        })
        .collect();
    gojson::sorted(&json!({ "object": "list", "data": data }))
}

/// Go `grokbuild.BuildResponse` over `GetAvailableModelInfos` (sorted by trimmed ID),
/// marshalled in struct field order.
pub fn grok_list<'a>(models: impl Iterator<Item = &'a Spec>) -> String {
    let mut models: Vec<&Spec> = models.collect();
    models.sort_by(|a, b| a.id.trim().cmp(b.id.trim()));
    let data: Vec<String> = models
        .into_iter()
        .map(|m| {
            let name = if m.display_name.is_empty() {
                &m.id
            } else {
                &m.display_name
            };
            let mut entry = gojson::Obj::new()
                .str("id", &m.id)
                .str("model", &m.id)
                .str("name", name);
            if m.context_length > 0 {
                entry = entry.raw("context_window", &m.context_length.to_string());
            }
            entry = entry.str("api_backend", "responses").raw("supported_in_api", "true");
            let efforts: Vec<String> = m
                .thinking
                .iter()
                .flat_map(|t| &t.levels)
                .map(|level| level.trim())
                .filter(|level| !level.is_empty())
                .map(|level| gojson::Obj::new().str("value", level).finish())
                .collect();
            if !efforts.is_empty() {
                entry = entry.raw("reasoning_efforts", &format!("[{}]", efforts.join(",")));
            }
            entry.finish()
        })
        .collect();
    gojson::Obj::new()
        .str("object", "list")
        .raw("data", &format!("[{}]", data.join(",")))
        .finish()
}

/// Go `convertModelToMap(model, "claude")` plus `claudemodels.BuildResponse`.
pub fn claude_list<'a>(models: impl Iterator<Item = &'a Spec>, disable_cloaking: bool) -> String {
    let mut data: Vec<(String, String, Value)> = models
        .map(|m| {
            let id = if disable_cloaking {
                m.id.clone()
            } else {
                crate::claude::ensure_dd(&m.id)
            };
            let display = if m.display_name.is_empty() {
                m.id.clone()
            } else {
                m.display_name.clone()
            };
            let mut entry = Map::new();
            entry.insert("id".into(), id.clone().into());
            entry.insert("object".into(), "model".into());
            entry.insert("owned_by".into(), m.owned_by.clone().into());
            if m.created > 0 {
                entry.insert("created_at".into(), rfc3339(m.created).into());
            }
            entry.insert("type".into(), "model".into());
            entry.insert("display_name".into(), display.clone().into());
            let input = if m.context_length > 0 {
                m.context_length
            } else {
                CLAUDE_MAX_INPUT
            };
            let output = if m.max_completion_tokens > 0 {
                m.max_completion_tokens
            } else {
                CLAUDE_MAX_OUTPUT
            };
            entry.insert("max_input_tokens".into(), input.into());
            entry.insert("max_tokens".into(), output.into());
            (display, id, Value::Object(entry))
        })
        .collect();
    data.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let first = data.first().map(|d| d.1.clone()).unwrap_or_default();
    let last = data.last().map(|d| d.1.clone()).unwrap_or_default();
    let data: Vec<Value> = data.into_iter().map(|d| d.2).collect();
    gojson::sorted(&json!({ "data": data, "has_more": false, "first_id": first, "last_id": last }))
}

/// `time.Unix(s, 0).UTC().Format(time.RFC3339)`.
pub fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_go() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_759_276_800), "2025-10-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
    }

    #[test]
    fn list_shapes_follow_go_marshal() {
        let spec = Spec {
            id: "gpt-x".into(),
            owned_by: "openai".into(),
            created: 5,
            display_name: "B".into(),
            ..Spec::default()
        };
        let claude = Spec {
            id: "claude-a".into(),
            owned_by: "anthropic".into(),
            display_name: "A".into(),
            context_length: 10,
            ..Spec::default()
        };
        assert_eq!(
            openai_list([&spec].into_iter()),
            r#"{"data":[{"created":5,"id":"gpt-x","object":"model","owned_by":"openai"}],"object":"list"}"#
        );
        assert_eq!(
            claude_list([&spec, &claude].into_iter(), false),
            r#"{"data":[{"display_name":"A","id":"claude-a","max_input_tokens":10,"max_tokens":64000,"object":"model","owned_by":"anthropic","type":"model"},{"created_at":"1970-01-01T00:00:05Z","display_name":"B","id":"claude-fable-5-dd-x-tpg","max_input_tokens":200000,"max_tokens":64000,"object":"model","owned_by":"openai","type":"model"}],"first_id":"claude-a","has_more":false,"last_id":"claude-fable-5-dd-x-tpg"}"#
        );
    }

    /// Go `WriteModelListResponse`'s detail selection, recorded with encoding/json (Go
    /// 1.26): keys fold, the last match wins, a mistyped field fails its whole decode.
    #[test]
    fn detail_selection_decodes_the_catalog_like_go() {
        for (body, id, want) in [
            (
                r#"{"Data":[{"id":"a" , "n":1.50}]}"#,
                "a",
                Ok(Some(r#"{"id":"a" , "n":1.50}"#)),
            ),
            (r#"{"data":[{"id":"a"}],"DATA":null}"#, "a", Ok(None)),
            (r#"{"data":"x","models":[{"id":"a"}]}"#, "a", Err(())),
            (
                r#"{"data":[{"id":1},{"ID":"a","id":null}]}"#,
                "a",
                Ok(Some(r#"{"ID":"a","id":null}"#)),
            ),
            (
                r#"{"models":[{"id":"","slug":"s"}],"data":[{"slug":"s","id":"t"}]}"#,
                "s",
                Ok(Some(r#"{"id":"","slug":"s"}"#)),
            ),
            (r#"{"data":[{"id":"a","slug":7}]}"#, "a", Ok(None)),
            ("[]", "a", Err(())),
            ("null", "a", Ok(None)),
            (r#"{"data":[{"id":"\u0061"}]}"#, "a", Ok(Some(r#"{"id":"\u0061"}"#))),
            (r#"{"data":[{"id":"k"}]}"#, "\u{212a}", Ok(None)),
            (r#"{"dat\u0061":[{"id":"a"}]}"#, "a", Ok(Some(r#"{"id":"a"}"#))),
        ] {
            let got = select_detail(body.as_bytes(), id).map(|e| e.map(|e| std::str::from_utf8(e).unwrap()));
            assert_eq!(got, want, "{body}");
        }
    }
}
