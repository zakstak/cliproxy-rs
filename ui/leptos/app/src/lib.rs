use gloo_net::http::Request;
use ledger_model::{Activity, ActivityPage, ConfigSnapshot, Editor, Outcome, WritePhase};
use leptos::{prelude::*, task::spawn_local};
use serde::de::DeserializeOwned;
use wasm_bindgen::{JsCast, prelude::*};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Activity,
    Config,
}

#[derive(Clone, Copy)]
struct AppState {
    page: RwSignal<Page>,
    rows: RwSignal<Vec<Activity>>,
    cursor: RwSignal<u64>,
    selected: RwSignal<Option<Activity>>,
    paused: RwSignal<bool>,
    fetching: RwSignal<bool>,
    feed_error: RwSignal<Option<String>>,
    gap: RwSignal<bool>,
    search: RwSignal<String>,
    errors_only: RwSignal<bool>,
    editor: RwSignal<Option<Editor>>,
    config_error: RwSignal<Option<String>>,
    notice: RwSignal<String>,
    light: RwSignal<bool>,
}

async fn json<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let response = Request::get(path).send().await.map_err(|error| error.to_string())?;
    if !response.ok() {
        return Err(format!("Read failed (HTTP {}).", response.status()));
    }
    response.json().await.map_err(|error| error.to_string())
}

fn focus(id: &str) {
    // Wait until Leptos has committed the conditional controls before restoring focus.
    let id = id.to_owned();
    request_animation_frame(move || {
        if let Some(element) = document()
            .get_element_by_id(&id)
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let _ = element.focus();
        }
    });
}

fn refresh(state: AppState) {
    if state.fetching.get_untracked() {
        return;
    }
    state.fetching.set(true);
    spawn_local(async move {
        let cursor = state.cursor.get_untracked();
        match json::<ActivityPage>(&format!("/sandbox/activity?after={cursor}")).await {
            Ok(page) => {
                state.rows.update(|rows| {
                    rows.extend(page.records);
                    if rows.len() > 80 {
                        rows.drain(..rows.len() - 80);
                    }
                });
                state.cursor.set(page.cursor);
                if page.gap {
                    state.gap.set(true);
                }
                state.feed_error.set(None);
            }
            Err(error) => state.feed_error.set(Some(error)),
        }
        state.fetching.set(false);
    });
}

fn load_config(state: AppState) {
    spawn_local(async move {
        match json::<ConfigSnapshot>("/sandbox/config").await {
            Ok(snapshot) => {
                state.editor.set(Some(Editor::new(snapshot)));
                state.config_error.set(None);
            }
            Err(error) => state.config_error.set(Some(error)),
        }
    });
}

fn save(state: AppState) {
    let mut reviewed = None;
    state.editor.update(|editor| {
        if let Some(editor) = editor {
            reviewed = editor.begin_save();
        }
    });
    let Some(reviewed) = reviewed else { return };
    state.config_error.set(None);
    spawn_local(async move {
        let result = async {
            let response = Request::put("/sandbox/config")
                .header("Content-Type", "text/yaml")
                .header("If-Match", &reviewed.base.etag())
                .body(reviewed.yaml)
                .map_err(|error| error.to_string())?
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let status = response.status();
            if status != 200 && status != 412 {
                return Err(format!("Save failed (HTTP {status}). Your draft is retained."));
            }
            let snapshot = response
                .json::<ConfigSnapshot>()
                .await
                .map_err(|error| error.to_string())?;
            Ok((status, snapshot))
        }
        .await;
        match result {
            Ok((200, snapshot)) => {
                let version = snapshot.version;
                state.editor.update(|editor| {
                    if let Some(editor) = editor {
                        editor.saved(snapshot);
                    }
                });
                state.notice.set(format!("Saved revision {version} to the sandbox."));
                focus("config-heading");
            }
            Ok((_, snapshot)) => {
                state.editor.update(|editor| {
                    if let Some(editor) = editor {
                        editor.conflict(snapshot);
                    }
                });
                state
                    .notice
                    .set("Another operator saved a change. Your draft is retained.".into());
                focus("conflict-heading");
            }
            Err(error) => {
                state.editor.update(|editor| {
                    if let Some(editor) = editor {
                        editor.retry_after_error();
                    }
                });
                state.config_error.set(Some(error));
            }
        }
    });
}

#[wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    let state = AppState {
        page: RwSignal::new(Page::Activity),
        rows: RwSignal::new(Vec::new()),
        cursor: RwSignal::new(0),
        selected: RwSignal::new(None),
        paused: RwSignal::new(false),
        fetching: RwSignal::new(false),
        feed_error: RwSignal::new(None),
        gap: RwSignal::new(false),
        search: RwSignal::new(String::new()),
        errors_only: RwSignal::new(false),
        editor: RwSignal::new(None),
        config_error: RwSignal::new(None),
        notice: RwSignal::new(String::new()),
        light: RwSignal::new(false),
    };
    refresh(state);
    load_config(state);
    let timer = gloo_timers::callback::Interval::new(3000, move || {
        if !state.paused.get_untracked() && state.page.get_untracked() == Page::Activity {
            refresh(state);
        }
    });
    let unload = Closure::<dyn FnMut(web_sys::BeforeUnloadEvent)>::new(move |event: web_sys::BeforeUnloadEvent| {
        if state
            .editor
            .with_untracked(|editor| editor.as_ref().is_some_and(Editor::dirty))
        {
            event.prevent_default();
            event.set_return_value("");
        }
    });
    let _ = window().add_event_listener_with_callback("beforeunload", unload.as_ref().unchecked_ref());
    let browser_handles = StoredValue::new_local((timer, unload));
    on_cleanup(move || {
        browser_handles.with_value(|(_, unload)| {
            let _ = window().remove_event_listener_with_callback("beforeunload", unload.as_ref().unchecked_ref());
        });
    });
    Effect::new(move |_| {
        let _ = document()
            .document_element()
            .map(|root| root.set_attribute("data-theme", if state.light.get() { "light" } else { "dark" }));
    });
    let navigate = move |page| {
        state.page.set(page);
        focus("page-heading");
    };
    view! {
        <a class="skip" href="#main">"Skip to content"</a>
        <header class="topbar">
            <a class="wordmark" href="/" aria-label="Ledger sandbox home"><span class="mark">"L"</span>"ledger"</a>
            <nav aria-label="Dashboard">
                <button class:active=move || state.page.get() == Page::Activity aria-current=move || if state.page.get() == Page::Activity { Some("page") } else { None }
                    on:click=move |_| navigate(Page::Activity)>"Activity"</button>
                <button class:active=move || state.page.get() == Page::Config aria-current=move || if state.page.get() == Page::Config { Some("page") } else { None }
                    on:click=move |_| navigate(Page::Config)>"Configuration"<Show when=move || state.editor.with(|ed| ed.as_ref().is_some_and(Editor::dirty))><span class="draft-dot" aria-label="Unsaved draft"></span></Show></button>
            </nav>
            <span class="sandbox-tag">"Sandbox"</span>
            <button class="theme-button" aria-label=move || if state.light.get() { "Use dark theme" } else { "Use light theme" }
                on:click=move |_| state.light.update(|value| *value = !*value)>{move || if state.light.get() { "Dark" } else { "Light" }}</button>
        </header>
        <main id="main" tabindex="-1">
            <div class="page-intro"><div><p class="eyebrow">"CLIPROXY-RS / LEDGER"</p>
                <h1 id="page-heading" tabindex="-1">{move || match state.page.get() { Page::Activity => "Activity", Page::Config => "Configuration" }}</h1>
                <p class="subtitle">{move || match state.page.get() { Page::Activity => "Every request, from client to provider.", Page::Config => "Review your changes before they reach the server." }}</p>
            </div><div class="environment"><span class="status-dot"></span><div><strong>"Disposable workspace"</strong><span>"In-memory data · resets on restart"</span></div></div></div>
            <div role="status" aria-live="polite" class="announcement">{move || state.notice.get()}</div>
            <section hidden=move || state.page.get() != Page::Activity aria-label="Request activity"><ActivityView state/></section>
            <section hidden=move || state.page.get() != Page::Config aria-label="Configuration editor"><ConfigView state/></section>
            <footer><span>"Ledger / Leptos"</span><span>"Fixture data only. No provider accounts connected."</span></footer>
        </main>
    }
}

#[component]
fn ActivityView(state: AppState) -> impl IntoView {
    let filtered = move || {
        let search = state.search.get().to_lowercase();
        let errors = state.errors_only.get();
        state
            .rows
            .get()
            .into_iter()
            .rev()
            .filter(|row| {
                (!errors || row.outcome != Outcome::Completed)
                    && (search.is_empty()
                        || format!("{} {} {} {}", row.client, row.provider, row.model, row.id)
                            .to_lowercase()
                            .contains(&search))
            })
            .collect::<Vec<_>>()
    };
    view! {
        <div class="metrics">
            <div><span>"Requests watched"</span><strong>{move || state.rows.with(Vec::len)}</strong><small>"Retained in this tab"</small></div>
            <div><span>"Completed"</span><strong>{move || state.rows.with(|rows| rows.iter().filter(|row| row.outcome == Outcome::Completed).count())}</strong><small>"Returned successfully"</small></div>
            <div><span>"Need attention"</span><strong class="warn">{move || state.rows.with(|rows| rows.iter().filter(|row| row.outcome != Outcome::Completed).count())}</strong><small>"Limited or failed"</small></div>
            <div><span>"Cache reads"</span><strong>{move || format!("{:.1}k", state.rows.with(|rows| rows.iter().map(|row| u64::from(row.cache_read_tokens)).sum::<u64>()) as f64 / 1000.0)}</strong><small>"Tokens reused"</small></div>
        </div>
        <div class="workspace" class:has-detail=move || state.selected.get().is_some()>
            <div class="panel requests">
                <div class="panel-title"><h2>"Requests" <span class="count">{move || filtered().len()}</span></h2>
                    <button class="watch" class:paused=move || state.paused.get() aria-pressed=move || state.paused.get().to_string()
                        on:click=move |_| { state.paused.update(|value| *value = !*value); if !state.paused.get_untracked() { refresh(state); } }>
                        <span class="status-dot"></span>{move || if state.paused.get() { "Resume watching" } else { "Pause watching" }}</button></div>
                <div class="filters"><label class="search"><span class="sr-only">"Search requests"</span><input type="search" placeholder="Search client, model, provider…" prop:value=move || state.search.get() on:input=move |ev| state.search.set(event_target_value(&ev))/></label>
                    <label class="checkbox"><input type="checkbox" prop:checked=move || state.errors_only.get() on:change=move |ev| state.errors_only.set(event_target_checked(&ev))/>"Needs attention"</label></div>
                <Show when=move || state.feed_error.get().is_some()><div class="notice error" role="alert"><div><strong>"Activity is unavailable"</strong><p>"Your last good records are retained. "{move || state.feed_error.get().unwrap_or_default()}</p></div><button on:click=move |_| refresh(state)>"Retry"</button></div></Show>
                <Show when=move || state.gap.get()><div class="notice">"Some requests fell outside the feed’s retention window. Counts cover retained records only."</div></Show>
                <div class="table-scroll"><table><thead><tr><th>"Time"</th><th>"Client / model"</th><th>"Provider"</th><th>"Status"</th><th class="numeric">"Duration"</th><th class="numeric">"Tokens"</th></tr></thead>
                    <tbody><For each=filtered key=|row| row.id children=move |row| {
                        let selected = row.clone();
                        let id = row.id;
                        view! { <tr class:selected=move || state.selected.with(|value| value.as_ref().is_some_and(|row| row.id == id))>
                            <td class="mono muted">{row.time}</td><td><button class="row-link" id=format!("request-{id}") on:click=move |_| { state.selected.set(Some(selected.clone())); focus("detail-heading"); }><strong>{row.client}</strong><span class="mono">{row.model}</span></button></td>
                            <td>{row.provider}</td><td><span class=format!("badge {}", row.outcome.class())>{row.outcome.label()}</span></td>
                            <td class="numeric mono">{format!("{:.2}s", f64::from(row.duration_ms) / 1000.0)}</td><td class="numeric mono">{format!("{:.1}k", f64::from(row.input_tokens + row.output_tokens) / 1000.0)}</td>
                        </tr> }
                    }/></tbody></table></div>
                <Show when=move || filtered().is_empty()><div class="empty"><h3>{move || if state.rows.with(Vec::is_empty) { "Waiting for requests" } else { "No matching requests" }}</h3><p>"Try another search or turn off the attention filter."</p></div></Show>
                <div class="panel-foot"><span>{move || if state.paused.get() { "Paused · records retained" } else { "Watching · refreshes every 3 seconds" }}</span><span>"Latest first · up to 80 records"</span></div>
            </div>
            {move || state.selected.get().map(|row| view! { <RequestDetail row state/> })}
        </div>
        <details class="scenarios"><summary>"Try a failure scenario"</summary><p>"These controls affect only this disposable sandbox."</p><button on:click=move |_| spawn_local(async move {
            match Request::post("/sandbox/activity/fail-next").send().await {
                Ok(response) if response.ok() => refresh(state),
                _ => state.feed_error.set(Some("Could not trigger the sandbox scenario.".into())),
            }
        })>"Fail the next Activity read"</button></details>
    }
}

#[component]
fn RequestDetail(row: Activity, state: AppState) -> impl IntoView {
    let id = row.id;
    let close = move || {
        state.selected.set(None);
        focus(&format!("request-{id}"));
    };
    view! {
        <aside class="panel detail" aria-labelledby="detail-heading" on:keydown=move |event| { if event.key() == "Escape" { close(); } }>
            <div class="panel-title"><h2 id="detail-heading" tabindex="-1">{format!("Request #{id}")}</h2><button aria-label="Close request details" on:click=move |_| close()>"Close"</button></div>
            <div class="detail-body"><span class=format!("badge {}", row.outcome.class())>{row.outcome.label()}</span><h3>{row.client}</h3><p class="mono muted">{row.model}</p>
                <dl><div><dt>"Provider"</dt><dd>{row.provider}</dd></div><div><dt>"Duration"</dt><dd>{format!("{} ms", row.duration_ms)}</dd></div><div><dt>"Input tokens"</dt><dd>{row.input_tokens}</dd></div><div><dt>"Output tokens"</dt><dd>{row.output_tokens}</dd></div><div><dt>"Cache read tokens"</dt><dd>{row.cache_read_tokens}</dd></div></dl>
                <p class="detail-note">{match row.outcome { Outcome::Completed => "The fixture returned a complete response.", Outcome::Limited => "The fixture account reached a provider limit. Another account can serve the next request.", Outcome::Failed => "The fixture upstream ended the request. This example contains no prompt or response content." }}</p>
            </div>
        </aside>
    }
}

#[component]
fn ConfigView(state: AppState) -> impl IntoView {
    let editing = move || {
        state
            .editor
            .with(|editor| editor.as_ref().is_some_and(|ed| ed.phase == WritePhase::Editing))
    };
    let busy = move || {
        state.editor.with(|editor| {
            editor
                .as_ref()
                .is_some_and(|ed| matches!(ed.phase, WritePhase::Saving(_)))
        })
    };
    view! {
        <div class="config-layout"><div class="panel editor-panel">
            <div class="panel-title"><h2 id="config-heading" tabindex="-1">"config.yaml"<span class="count">{move || state.editor.with(|editor| editor.as_ref().map(|ed| format!("revision {}", ed.base.version)).unwrap_or_else(|| "Loading…".into()))}</span></h2>
                <span class="draft-label">{move || state.editor.with(|editor| if editor.as_ref().is_some_and(Editor::dirty) { "Unsaved draft" } else { "Up to date" })}</span></div>
            <div class="editor-body"><label for="config-text">"YAML configuration"</label><p id="config-hint" class="muted">"Drafts stay here when you switch screens. Comments and formatting are preserved."</p>
                <textarea id="config-text" rows="17" spellcheck="false" aria-describedby="config-hint config-error" aria-invalid=move || state.config_error.get().is_some().to_string()
                    readonly=move || !editing() prop:value=move || state.editor.with(|ed| ed.as_ref().map(|ed| ed.text.clone()).unwrap_or_default())
                    on:input=move |event| { state.config_error.set(None); state.notice.set(String::new()); state.editor.update(|ed| { if let Some(ed) = ed { ed.edit(event_target_value(&event)); } }); }></textarea>
                <div id="config-error" role="alert" class="validation">{move || state.config_error.get().unwrap_or_default()}</div>
                <Show when=move || state.editor.get().is_none()><button on:click=move |_| load_config(state)>"Retry loading configuration"</button></Show>
                <Show when=editing><div class="actions"><button class="primary" disabled=move || !state.editor.with(|ed| ed.as_ref().is_some_and(Editor::dirty)) on:click=move |_| {
                    state.config_error.set(None);
                    state.editor.update(|ed| { if let Some(ed) = ed && let Err(error) = ed.review() { state.config_error.set(Some(error)); } });
                    if state.config_error.get_untracked().is_none() { focus("review-heading"); }
                }>"Review changes"</button>
                <Show when=move || state.editor.with(|ed| ed.as_ref().is_some_and(|ed| !ed.history.is_empty()))><button on:click=move |_| { state.editor.update(|ed| { if let Some(ed) = ed { ed.restore_previous(); } }); focus("config-text"); }>"Restore previous draft"</button></Show>
                </div></Show>
            </div>
        </div><aside class="config-guide"><p class="eyebrow">"A REVIEWED WRITE"</p><h2>"Your draft, protected."</h2><ol><li><strong>"Edit locally"</strong><p>"Check YAML before sending anything."</p></li><li><strong>"Review the exact change"</strong><p>"The reviewed text and base revision are frozen together."</p></li><li><strong>"Save against that revision"</strong><p>"A competing write opens reconciliation. Your draft stays intact."</p></li></ol>
            <details class="scenarios"><summary>"Try a conflicting write"</summary><p>"Edit and review your draft, then simulate another operator saving before you apply it."</p><button disabled=busy on:click=move |_| spawn_local(async move {
                match Request::post("/sandbox/config/competing-write").send().await {
                    Ok(response) if response.ok() => state.notice.set("Another operator saved a revision. Apply your reviewed change to test conflict protection.".into()),
                    _ => state.config_error.set(Some("Could not simulate another write.".into())),
                }
            })>"Simulate another operator saving"</button></details>
        </aside></div>
        {move || state.editor.get().map(|ed| match ed.phase {
            WritePhase::Editing => ().into_any(),
            WritePhase::Reviewing(review) | WritePhase::Saving(review) => view! {
                <section class="panel review" aria-labelledby="review-heading"><div class="panel-title"><h2 id="review-heading" tabindex="-1">"Review changes"</h2><span class="muted">{format!("Based on revision {}", review.base.version)}</span></div>
                    <div class="comparison"><div><h3>"Server version"</h3><pre>{review.base.yaml}</pre></div><div><h3>"Your proposed version"</h3><pre>{review.yaml}</pre></div></div>
                    <div class="review-actions"><button disabled=busy on:click=move |_| { state.editor.update(|ed| { if let Some(ed) = ed { ed.back_to_edit(); } }); focus("config-text"); }>"Back to edit"</button>
                        <button class="primary" disabled=busy on:click=move |_| save(state)>{move || if busy() { "Saving…" } else { "Apply reviewed changes" }}</button></div>
                </section>
            }.into_any(),
            WritePhase::Conflict { review, latest } => view! {
                <section class="panel conflict" aria-labelledby="conflict-heading"><div class="panel-title"><h2 id="conflict-heading" tabindex="-1">"The server changed"</h2><span class="badge warn">"Conflict"</span></div>
                    <p class="conflict-copy">{format!("You reviewed revision {}. The server is now at revision {}. Compare both versions before continuing.", review.base.version, latest.version)}</p>
                    <div class="comparison"><div><h3>"Latest server version"</h3><pre>{latest.yaml}</pre></div><div><h3>"Your draft · retained"</h3><pre>{review.yaml}</pre></div></div>
                    <div class="review-actions"><button on:click=move |_| { state.editor.update(|ed| { if let Some(ed) = ed { ed.reconcile(false); } }); state.notice.set("Using the server version. Your previous draft is available to restore.".into()); focus("config-text"); }>"Use server version"</button>
                        <button class="primary" on:click=move |_| { state.editor.update(|ed| { if let Some(ed) = ed { ed.reconcile(true); } }); state.notice.set("Your draft is retained against the latest revision. Review it again before saving.".into()); focus("config-text"); }>"Keep my draft and edit"</button></div>
                </section>
            }.into_any(),
        })}
    }
}
