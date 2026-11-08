//! Loopback-only upstreams for tests in other workspace crates.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// The failed count's body, kept exact for Home usage assertions.
pub const COUNT_DENIED: &str = r#"{"type":"error","error":{"type":"authentication_error","message":"bad token"}}"#;

/// A TLS mock for Claude's first-party count-token path. Its client trusts only a
/// throwaway CA, pins api.anthropic.com to loopback, and disables environment proxies.
/// Keys ending in fake-A receive 401; every other fake key receives a count of five.
pub struct CountTokensUpstream {
    client: wreq::Client,
    calls: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl CountTokensUpstream {
    pub async fn start() -> Self {
        let host = "api.anthropic.com";
        let (ca, acceptor) = crate::test_tls::acceptor(&[host.to_owned()], b"\x08http/1.1");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = wreq::Client::builder()
            .no_proxy()
            .http1_only()
            .resolve(host, listener.local_addr().unwrap())
            .tls_cert_store(wreq::tls::trust::CertStore::from_pem_stack(ca).unwrap())
            .build()
            .unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let acceptor = Arc::new(acceptor);
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (acceptor, seen) = (acceptor.clone(), seen.clone());
                tokio::spawn(async move {
                    tokio::time::timeout(std::time::Duration::from_secs(10), async move {
                        let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
                        let mut tls = tokio_btls::SslStream::new(ssl, socket).unwrap();
                        std::pin::Pin::new(&mut tls).accept().await.unwrap();
                        let mut io = BufReader::new(tls);
                        let mut line = String::new();
                        io.read_line(&mut line).await.unwrap();
                        assert!(line.starts_with("POST /v1/messages/count_tokens"), "{line}");
                        let (mut key, mut length) = (String::new(), 0usize);
                        loop {
                            line.clear();
                            assert!(io.read_line(&mut line).await.unwrap() > 0);
                            if line == "\r\n" { break; }
                            let (name, value) = line.split_once(':').unwrap();
                            if name.eq_ignore_ascii_case("x-api-key") || name.eq_ignore_ascii_case("authorization") {
                                key = value.trim().trim_start_matches("Bearer ").to_owned();
                            }
                            if name.eq_ignore_ascii_case("content-length") {
                                length = value.trim().parse().unwrap();
                            }
                        }
                        assert!(length > 0 && length < 1024 * 1024);
                        let mut body = vec![0; length];
                        io.read_exact(&mut body).await.unwrap();
                        assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
                        assert!(key.starts_with("sk-ant-api03-fake-"), "only fake fixture credentials");
                        seen.lock().unwrap().push(key.clone());
                        let (status, body) = if key.ends_with("fake-A") {
                            ("401 Unauthorized", COUNT_DENIED)
                        } else {
                            ("200 OK", r#"{"input_tokens":5}"#)
                        };
                        let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                        io.get_mut().write_all(response.as_bytes()).await.unwrap();
                        io.get_mut().shutdown().await.unwrap();
                    }).await.unwrap();
                });
            }
        });
        Self { client, calls, task }
    }

    pub fn executor(&self) -> crate::claude::ClaudeExecutor {
        crate::claude::ClaudeExecutor::with_client(self.client.clone(), "https://api.anthropic.com")
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Drop for CountTokensUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}
