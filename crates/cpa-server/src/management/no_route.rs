//! gin's NoRoute for the management router, and the plugin API that native plugin
//! removal retired (docs/DIFFERENCES-FROM-GO.md "Native plugins").

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use tower::ServiceExt as _;

use super::{Management, access};

/// The status of a retired plugin route: the operation existed and is gone for good.
const STATUS: StatusCode = StatusCode::GONE;

/// The JSON error every retired plugin route answers.
pub(crate) fn plugins_removed() -> Response {
    super::json(
        STATUS,
        &json!({
            "error": "plugins_removed",
            "message": "native plugin support was removed from cliproxy-rs: the plugin, plugin store, plugin quota \
                        and plugin resource routes no longer exist; see docs/MIGRATING-FROM-GO.md \"Native plugins\"",
        }),
    )
}

/// Whether `path` (decoded) is under a retired plugin management route, in either API
/// version: `plugins`, `plugin-store` and the plugin `quota` routes.
fn retired_management(path: &str) -> bool {
    let Some(rest) = path
        .strip_prefix("/v0/management/")
        .or_else(|| path.strip_prefix("/v8/management/"))
    else {
        return false;
    };
    let first = rest.split('/').next().unwrap_or_default();
    let v0 = path.starts_with("/v0/");
    first == "plugins" || (v0 && matches!(first, "plugin-store" | "quota"))
}

/// Go `pluginManagementNoRoute` without plugins: a retired plugin resource is answered
/// with [`plugins_removed`] (resources were public); unknown paths (and unregistered
/// methods on known ones) under `/v0/management` and the retired plugin management
/// routes go through the management guard first, then get a bare 404 or
/// [`plugins_removed`]; anything else is a bare 404.
pub(crate) async fn no_route(state: State<Arc<Management>>, req: Request) -> Response {
    crate::request_logging::unrouted(answer(state, req).await)
}

async fn answer(State(state): State<Arc<Management>>, req: Request) -> Response {
    // gin matches NoRoute prefixes on the decoded path.
    let path = super::percent_decode(req.uri().path());
    if path.starts_with("/v0/resource/plugins/") {
        return plugins_removed();
    }
    let retired = retired_management(&path);
    if !retired && path != "/v0/management" && !path.starts_with("/v0/management/") {
        return access::not_found();
    }
    let answer = move || async move {
        if retired {
            plugins_removed()
        } else {
            access::not_found()
        }
    };
    let inner: axum::routing::MethodRouter<(), std::convert::Infallible> = axum::routing::any(answer)
        .layer(axum::middleware::from_fn_with_state(state.clone(), access::guard))
        .with_state(state);
    match inner.oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    }
}

#[cfg(test)]
mod tests {
    use super::retired_management;

    #[test]
    fn retired_plugin_routes_are_recognised_in_both_versions() {
        for path in [
            "/v8/management/plugins",
            "/v8/management/plugins/acme",
            "/v8/management/plugins/store",
            "/v8/management/plugins/store/acme/install",
            "/v8/management/plugins/acme/quota",
            "/v0/management/plugins",
            "/v0/management/plugins/acme/config",
            "/v0/management/plugins/acme/enabled",
            "/v0/management/plugins/acme/quota/reset",
            "/v0/management/plugin-store",
            "/v0/management/plugin-store/acme/install",
            "/v0/management/quota/providers",
            "/v0/management/quota/fetch",
            "/v0/management/quota/reset",
        ] {
            assert!(retired_management(path), "{path}");
        }
        for path in [
            "/v8/management/config",
            "/v8/management/quota/fetch",
            "/v8/management/plugin-store",
            "/v0/management/plugins-extra",
            "/v0/management/reset-quota",
            "/v1/plugins",
        ] {
            assert!(!retired_management(path), "{path}");
        }
    }
}
