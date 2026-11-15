//! Terminal content and interaction contracts rendered by ratatui TestBackend.
use std::sync::Arc;

use serde_json::{Map, Value};

use super::app::{self, App};
use super::client::{Client, Object};
use super::{Msg, auth, config, dashboard, i18n, keys, logs, oauth};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/tui_inputs.json")).expect("fixture JSON")
}

fn text(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
    let mut out: Vec<String> = lines
        .iter()
        .map(|l| {
            let s: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            s.trim_end_matches(' ').to_owned()
        })
        .collect();
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

fn objects(v: &Value) -> Vec<Object> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|o| o.as_object().cloned().unwrap_or_default())
        .collect()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|s| s.as_str().unwrap_or_default().to_owned())
        .collect()
}

fn presses(n: Option<&Value>) -> usize {
    n.and_then(Value::as_u64).unwrap_or(0) as usize
}

fn render(case: &Map<String, Value>) -> Vec<String> {
    let client = Arc::new(Client::new("http://127.0.0.1:8317", ""));
    let width = case["width"].as_u64().unwrap() as usize;
    let input = case["input"].as_object().unwrap();
    let has = |k: &str| input.contains_key(k);
    match case["tab"].as_str().unwrap() {
        "dashboard" => {
            let mut tab = dashboard::Dashboard::new(client);
            tab.set_size(width, 1000);
            tab.data(dashboard::Data {
                config: input["config"].as_object().cloned(),
                auth_files: objects(&input["files"]),
                api_keys: strings(&input["keys"]),
                // What cliproxy-rs answers for an unset strategy; a configured one is
                // shown as configured, as in Go.
                strategy: Some("soonest-reset".into()),
                err: None,
            });
            if let Some(e) = input.get("error").and_then(Value::as_str) {
                tab.data(dashboard::Data {
                    config: None,
                    auth_files: Vec::new(),
                    api_keys: Vec::new(),
                    strategy: None,
                    err: Some(e.into()),
                });
            }
            tab.vp.text()
        }
        "config" => {
            let mut tab = config::ConfigTab::new(client);
            tab.set_size(width, 1000);
            tab.data(Ok(input["config"].as_object().cloned().unwrap_or_default()));
            for _ in 0..presses(input.get("down")) {
                tab.key("down");
            }
            if let Some(e) = input.get("update_error").and_then(Value::as_str) {
                tab.updated(Some(e.into()));
            }
            if has("update_ok") {
                tab.updated(None);
            }
            if let Some(v) = input.get("edit").and_then(Value::as_str) {
                tab.key("enter");
                tab.input.set_value(v);
                tab.key("end");
            }
            tab.vp.text()
        }
        "auth" => {
            let mut tab = auth::AuthTab::new(client);
            tab.set_size(width, 1000);
            tab.files(Ok(objects(&input["files"])));
            for _ in 0..presses(input.get("down")) {
                tab.key("down");
            }
            if has("expand") {
                tab.key("enter");
            }
            if has("confirm") {
                tab.key("d");
            }
            if let Some(v) = input.get("action").and_then(Value::as_str) {
                tab.action(Ok(v.into()));
            }
            if let Some(v) = input.get("edit").and_then(Value::as_str) {
                tab.key(v);
            }
            tab.vp.text()
        }
        "keys" => {
            let mut tab = keys::KeysTab::new(client);
            tab.set_size(width, 1000);
            let providers = keys::PROVIDERS
                .iter()
                .map(|(_, route)| objects(&input.get(*route).cloned().unwrap_or_default()))
                .collect();
            tab.data(Ok(keys::Data {
                api_keys: strings(&input["keys"]),
                providers,
            }));
            for _ in 0..presses(input.get("down")) {
                tab.key("down");
            }
            if has("confirm") {
                tab.key("d");
            }
            if has("add") {
                tab.key("a");
            }
            if let Some(e) = input.get("error").and_then(Value::as_str) {
                tab.action(Err(e.into()));
            }
            tab.vp.text()
        }
        "oauth" => {
            let mut tab = oauth::OAuthTab::new(None);
            tab.set_size(width, 1000);
            for _ in 0..presses(input.get("down")) {
                tab.key("down");
            }
            if let Some(start) = input.get("start").and_then(Value::as_object) {
                tab.generation = 1;
                tab.start(oauth::Start {
                    url: start["url"].as_str().unwrap().into(),
                    state: "st".into(),
                    provider: start["provider"].as_str().unwrap().into(),
                    user_code: start["user_code"].as_str().unwrap().into(),
                    device: start["device"].as_bool().unwrap(),
                    expires_in: start["expires_in"].as_i64().unwrap(),
                    generation: 1,
                    err: None,
                });
            }
            if let Some(e) = input.get("poll_error").and_then(Value::as_str) {
                tab.polled(oauth::Poll {
                    state: "st".into(),
                    generation: 1,
                    done: false,
                    message: String::new(),
                    err: Some(e.into()),
                });
            }
            tab.vp.text()
        }
        "logs" => {
            let mut tab = logs::LogsTab::new(client, None);
            tab.set_size(width, 1000);
            tab.message(Msg::LogsPoll(Ok((strings(&input["lines"]), 5))));
            if let Some(v) = input.get("filter").and_then(Value::as_str) {
                tab.key(v);
            }
            if has("pause") {
                tab.key("a");
            }
            tab.vp.text()
        }
        "gate" => {
            let mut app = App::new("http://127.0.0.1:8317", input["password"].as_str().unwrap(), None);
            if let Some(e) = input.get("error").and_then(Value::as_str) {
                app.auth_error = e.into();
            }
            text(&app.gate_doc().0)
        }
        "bars" => {
            let mut app = App::new("http://127.0.0.1:8317", "", None);
            app.authenticated = true;
            if input.get("logs").and_then(Value::as_bool) == Some(false) {
                app.logs_enabled = false;
                app.refresh_tabs();
            }
            app.active = input["active"].as_u64().unwrap() as usize;
            let bar = text(&[app.tab_bar()]).concat();
            vec![bar, app::status_text(width as u16).trim_end_matches(' ').to_owned()]
        }
        other => panic!("unknown tab {other}"),
    }
}

#[test]
fn tabs_render_visible_state_actions_and_masked_secrets() {
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
    let _locale = i18n::LOCALE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    struct RestoreLocale;
    impl Drop for RestoreLocale {
        fn drop(&mut self) {
            i18n::set_chinese(false);
        }
    }
    let _restore_locale = RestoreLocale;
    for case in fixture()["cases"].as_array().unwrap() {
        let case = case.as_object().unwrap();
        let name = case["name"].as_str().unwrap();
        i18n::set_chinese(case["locale"] == "zh");
        let lines = render(case);
        let content = lines.join("\n");
        let mut terminal = Terminal::new(TestBackend::new(case["width"].as_u64().unwrap() as u16, 200)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(content.as_str()), frame.area()))
            .unwrap();
        assert!(!content.trim().is_empty(), "{name}");
        let required: &[&str] = match name {
            "dashboard-en" => &["Dashboard", "fill-first", "socks5://127.0.0.1:1080"],
            "dashboard-zh" => &["仪表盘", "soonest-reset"],
            "dashboard-narrow" => &["Dashboard", "soonest-reset"],
            "dashboard-error" => &["401", "invalid management key"],
            "config-en" => &["Configuration", "round-robin"],
            "config-zh-updated" => &["配置", "更新成功"],
            "config-update-error" => &["400", "invalid body"],
            "config-editing" => &["Configuration", "7"],
            "config-empty" => &["Configuration"],
            "auth-expanded" => &["Auth Files", "codex", "token expired"],
            "auth-confirm" => &["认证文件", "claude"],
            "auth-action" => &["Deleted x.json"],
            "auth-edit" => &["Auth Files", "claude"],
            "auth-empty" => &["No auth files found"],
            "keys" => &[
                "API Keys",
                "Claude API Keys",
                "Codex API Keys",
                "xAI API Keys",
                "Meta API Keys",
            ],
            "keys-add-zh" => &["API 密钥"],
            "keys-error" => &["400", "invalid body"],
            "oauth-idle" => &["OAuth Login", "Claude", "Codex", "xAI", "Meta"],
            "oauth-web" => &["Codex", "auth.example.invalid"],
            "oauth-device-zh" => &["Meta", "ABCD-1234"],
            "oauth-error" => &["OAuth flow timed out"],
            "logs-warn" => &["slow upstream", "request failed"],
            "logs-paused-zh" => &["日志", "started"],
            "logs-empty" => &["Waiting for log output"],
            "gate-error" => &["Connect Management API", "401", "invalid management key"],
            "gate-zh" => &["连接管理 API"],
            "bars-wide" => &["Dashboard", "Config"],
            "bars-narrow" => &["Dashboard"],
            "bars-no-logs-zh" => &["仪表盘"],
            other => panic!("missing content contract for {other}"),
        };
        for value in required {
            assert!(content.contains(value), "{name}: missing {value:?}\n{content}");
        }
        for secret in ["sk-abcdefghijklmnop", "sk-ant-fake-123456789"] {
            assert!(!content.contains(secret), "{name}: credential displayed unmasked");
        }
        if name == "gate-zh" {
            assert!(!content.contains("secret"), "password input is masked");
        }
        if name == "logs-warn" {
            assert!(
                !content.contains("noise") && !content.contains("started"),
                "filter hides lower-severity lines"
            );
        }
        let buffer = terminal.backend().buffer();
        assert!(
            buffer.content.iter().any(|cell| cell.symbol() != " "),
            "{name}: terminal received visible content"
        );
        i18n::set_chinese(false);
    }
}
