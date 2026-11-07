//! The raw C ABI against a Go-built plugin: calls racing shutdown must either complete or
//! report a closed client, never touch freed library state.

// Native plugins load only on Unix (the host's `native` module).

mod support;

use std::sync::Arc;

use bytes::Bytes;
use cpa_plugin::client::{CallbackError, CallbackHandler, CallbackInstance};
use cpa_plugin::native::NativeClient;

struct NoCallbacks;

impl CallbackHandler for NoCallbacks {
    fn call_from_plugin(
        &self,
        _: &str,
        _: &Arc<CallbackInstance>,
        method: &str,
        _: Bytes,
    ) -> Result<Bytes, CallbackError> {
        Err(CallbackError::new(format!("unsupported host callback {method}")))
    }
}

#[test]
fn calls_racing_shutdown_are_safe() {
    let Some(built) = support::built_plugins_for("native_abi::calls_racing_shutdown_are_safe") else {
        return;
    };
    let client = Arc::new(
        NativeClient::open(
            &built.join("simple.so"),
            "simple",
            Arc::new(NoCallbacks),
            Arc::default(),
        )
        .unwrap(),
    );
    let first = client.call("auth.identifier", b"{}").unwrap();
    assert_eq!(&first[..], br#"{"ok":true,"result":{"identifier":"plugin-example"}}"#);
    // Each caller completes one call before shutdown starts (so `total` cannot be zero
    // on a slow machine), then keeps calling while shutdown races it.
    let started = Arc::new(std::sync::Barrier::new(5));
    let callers: Vec<_> = (0..4)
        .map(|_| {
            let client = client.clone();
            let started = started.clone();
            std::thread::spawn(move || {
                let first = client.call("model.static", b"{}").unwrap();
                assert!(first.starts_with(br#"{"ok":true"#));
                started.wait();
                let mut ok = 1;
                for _ in 0..500 {
                    match client.call("model.static", b"{}") {
                        Ok(resp) => {
                            assert!(resp.starts_with(br#"{"ok":true"#));
                            ok += 1;
                        }
                        Err(e) => {
                            assert_eq!(e, "plugin client is closed");
                            break;
                        }
                    }
                }
                ok
            })
        })
        .collect();
    started.wait();
    std::thread::sleep(std::time::Duration::from_millis(5));
    client.shutdown();
    let total: usize = callers.into_iter().map(|t| t.join().unwrap()).sum();
    assert!(total >= 4);
    assert_eq!(
        client.call("model.static", b"{}"),
        Err("plugin client is closed".into())
    );
}

#[test]
fn replacing_an_artifact_keeps_old_images_and_rejects_late_callbacks() {
    // A small native plugin makes each build distinguishable and exposes a late
    // callback, as a resident Go runtime can issue after logical shutdown.
    let root = support::scratch("native-shadow");
    let source = root.join("plugin.c");
    let artifact = root.join(format!(
        "plugin{}",
        cpa_plugin::platform::extension(cpa_plugin::platform::goos())
    ));
    std::fs::write(
        &source,
        r#"
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
typedef struct { void *ptr; size_t len; } buffer;
typedef struct {
    uint32_t version; void *ctx;
    int (*call)(void *, const char *, const uint8_t *, size_t, buffer *);
    void (*free_buffer)(void *, size_t);
} host_api;
typedef struct {
    uint32_t version;
    int (*call)(const char *, const uint8_t *, size_t, buffer *);
    void (*free_buffer)(void *, size_t);
    void (*shutdown)(void);
} plugin_api;
static const host_api *host;
static int shutdown_count;
static int late_callback(void) {
    buffer response = {0};
    int status = host->call(host->ctx, "host.log", NULL, 0, &response);
    if (response.ptr) host->free_buffer(response.ptr, response.len);
    return status;
}
static int call(const char *method, const uint8_t *request, size_t len, buffer *response) {
    (void)request; (void)len;
    uintptr_t value = strcmp(method, "late") == 0 ? (uintptr_t)late_callback : VERSION + shutdown_count;
    response->len = sizeof(value);
    response->ptr = malloc(sizeof(value));
    memcpy(response->ptr, &value, sizeof(value));
    return 0;
}
static void free_buffer(void *ptr, size_t len) { (void)len; free(ptr); }
static void shutdown_plugin(void) { shutdown_count++; }
int cliproxy_plugin_init(const host_api *in, plugin_api *out) {
    host = in;
    *out = (plugin_api){1, call, free_buffer, shutdown_plugin};
    return 0;
}
"#,
    )
    .unwrap();
    let build = |version| {
        let next = root.join("next.so");
        let status = std::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-o"])
            .arg(&next)
            .arg(&source)
            .arg(format!("-DVERSION={version}"))
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::rename(next, &artifact).unwrap();
    };
    let open = || NativeClient::open(&artifact, "shadow-test", Arc::new(NoCallbacks), Arc::default()).unwrap();
    let value = |client: &NativeClient, method| {
        usize::from_ne_bytes(client.call(method, b"{}").unwrap()[..].try_into().unwrap())
    };
    build(10);
    let first = open();
    // SAFETY: geteuid has no preconditions.
    let shadow_dir = std::env::temp_dir()
        .join(format!("cliproxy-pluginhost-{}", unsafe { libc::geteuid() }))
        .join(format!("pid-{}", std::process::id()));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(shadow_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(value(&first, "version"), 10);
    let late = value(&first, "late");
    build(20);
    let second = open();
    assert_eq!(
        value(&first, "version"),
        10,
        "replacing the source does not affect an active image"
    );
    assert_eq!(value(&second, "version"), 20, "a changed build maps as a fresh image");
    first.shutdown();
    first.shutdown();
    drop(first);
    // SAFETY: the test plugin returns its late_callback entry point as uintptr_t.
    // It remains callable after shutdown because the loader retains its image.
    let late: unsafe extern "C" fn() -> std::ffi::c_int = unsafe { std::mem::transmute(late) };
    assert_eq!(
        unsafe { late() },
        1,
        "a stale context fails lookup without touching a freed host"
    );
    assert_eq!(value(&second, "version"), 20);
    second.shutdown();
    drop(second);
    let reused = open();
    assert_eq!(
        value(&reused, "version"),
        21,
        "unchanged content reuses the resident image"
    );
    reused.shutdown();
    std::fs::remove_dir_all(root).unwrap();
}
