//! Go's remote config and credential stores (`PGSTORE_*`, `OBJECTSTORE_*`,
//! `GITSTORE_*`) are not supported (#222): the binary stops with the way forward,
//! never echoes the setting's value and starts nothing. No network beyond loopback.
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn scratch(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cliproxy-store-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The child process, killed and reaped even when an assertion fails first.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The binary with a clean environment and a private auth directory, so nothing reads
/// the real `~/.cli-proxy-api`. `-local-model` keeps the catalog updaters offline.
fn cliproxy(wd: &Path, env: &[(&str, &str)]) -> Server {
    std::fs::write(
        wd.join("config.yaml"),
        format!("port: 0\nauth-dir: {}\n", wd.join("auth").display()),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cliproxy"));
    cmd.arg("-local-model")
        .current_dir(wd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", wd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    Server(cmd.spawn().unwrap())
}

/// Everything the process printed, once it has exited.
fn output(server: &mut Server) -> String {
    let out = server.0.stdout.take().unwrap();
    let err = server.0.stderr.take().unwrap();
    let readers = [Box::new(out) as Box<dyn std::io::Read + Send>, Box::new(err)].map(|stream| {
        std::thread::spawn(move || BufReader::new(stream).lines().map_while(Result::ok).collect::<Vec<_>>())
    });
    // Generous: only stops a broken test from hanging on a server that kept running.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while server.0.try_wait().unwrap().is_none() {
        assert!(std::time::Instant::now() < deadline, "the server did not stop");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    readers.map(|r| r.join().unwrap().join("\n")).join("\n")
}

#[test]
fn a_go_remote_store_setting_stops_the_server_with_the_way_forward() {
    for (key, value) in [
        ("PGSTORE_DSN", "postgres://fake:fake-password@127.0.0.1:1/db"),
        ("OBJECTSTORE_ENDPOINT", "http://fake-password@127.0.0.1:1"),
        ("gitstore_git_url", "https://fake-password@127.0.0.1:1/repo.git"),
    ] {
        let wd = scratch("refused");
        let mut server = cliproxy(&wd, &[(key, value)]);
        let text = output(&mut server);
        assert!(text.contains(&key.to_ascii_uppercase()), "{key}: {text}");
        assert!(text.contains("issue #222"), "{key}: {text}");
        assert!(
            !text.contains("fake-password"),
            "{key}: the value is never echoed: {text}"
        );
        assert!(!text.contains("listening"), "{key}: nothing starts: {text}");
        let _ = std::fs::remove_dir_all(&wd);
    }
}

#[test]
fn a_blank_store_setting_selects_nothing() {
    let wd = scratch("blank");
    let mut server = cliproxy(&wd, &[("PGSTORE_DSN", "  ")]);
    let (tx, rx) = std::sync::mpsc::channel();
    for stream in [
        Box::new(server.0.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(server.0.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
    }
    let mut seen = Vec::new();
    while !seen.iter().any(|l: &String| l.contains("listening")) {
        match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(line) => seen.push(line),
            Err(_) => panic!("the server did not start:\n{}", seen.join("\n")),
        }
    }
    drop(server);
    let _ = std::fs::remove_dir_all(&wd);
}
