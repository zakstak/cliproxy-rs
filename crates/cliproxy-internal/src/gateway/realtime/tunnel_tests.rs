//! ICE-TCP authentication, public-address isolation and proxy protocol contracts.
//! Archived SDP and local-peer responses are inputs only; no proxy answer is replayed.

use std::sync::Mutex;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;

use super::*;
use crate::gateway::realtime::dialer::{Conn, build, proxy_scheme};

fn inputs() -> Value {
    serde_json::from_str(include_str!("../../../tests/gateway/fixtures/tunnel_inputs.json")).unwrap()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// RFC 5389 binding request with ICE short-term credentials, built by the Rust STUN
/// library. RFC 4571 supplies the two-byte network-order length prefix.
fn valid_frame() -> Vec<u8> {
    use rtc::stun::integrity::MessageIntegrity;
    use rtc::stun::message::Setter;
    let mut message = Message::new();
    message.typ = BINDING_REQUEST;
    message.write_header();
    message.add(ATTR_USERNAME, b"remote:local");
    let provider = rtc::crypto::default_provider().unwrap();
    MessageIntegrity::new_short_term_integrity_with_provider("remote-password".into(), provider.crypto())
        .add_to(&mut message)
        .unwrap();
    FINGERPRINT.add_to(&mut message).unwrap();
    let mut framed = (message.raw.len() as u16).to_be_bytes().to_vec();
    framed.extend(message.raw);
    framed
}

fn invalid_frames() -> Vec<Vec<u8>> {
    let valid = valid_frame();
    let mut cases = Vec::new();
    // Corrupt every authenticated byte, including the header, credential, MAC and CRC.
    for index in 2..valid.len() {
        let mut frame = valid.clone();
        frame[index] ^= 0x80;
        cases.push(frame);
    }
    for len in [0u16, 1, 19, 4097] {
        let mut frame = len.to_be_bytes().to_vec();
        frame.resize(2 + usize::from(len), 0);
        cases.push(frame);
    }
    for end in [0, 1, 2, 3, valid.len() - 1] {
        cases.push(valid[..end].to_vec());
    }
    cases
}

fn quiet() -> OnForwarding {
    Arc::new(|| {})
}

struct Never;

impl Dial for Never {
    fn dial(&self, _: SocketAddr) -> BoxFuture<'_, Result<Conn, String>> {
        Box::pin(async { Err("never".to_owned()) })
    }
}

/// Go's `recordingProxyDialer`: reports each dial and hands back the far end of a pipe,
/// or fails when `fail` is set.
struct Recording {
    dials: mpsc::UnboundedSender<(SocketAddr, Option<DuplexStream>)>,
    fail: bool,
}

impl Dial for Recording {
    fn dial(&self, target: SocketAddr) -> BoxFuture<'_, Result<Conn, String>> {
        Box::pin(async move {
            if self.fail {
                let _ = self.dials.send((target, None));
                return Err("proxy blocked".to_owned());
            }
            let (near, far) = tokio::io::duplex(4096);
            let _ = self.dials.send((target, Some(far)));
            Ok(Box::new(near) as Conn)
        })
    }
}

fn recording(
    fail: bool,
) -> (
    Arc<Recording>,
    mpsc::UnboundedReceiver<(SocketAddr, Option<DuplexStream>)>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(Recording { dials: tx, fail }), rx)
}

/// Counts forwarding-started calls.
fn counter() -> (OnForwarding, Arc<Mutex<u32>>) {
    let count = Arc::new(Mutex::new(0));
    let seen = count.clone();
    (Arc::new(move || *seen.lock().unwrap() += 1), count)
}

const TARGET: &str = "20.42.0.20:443";

/// A tunnel for the Go tests' fixed target and credentials; aborted on drop.
struct Open(SocketAddr, AbortHandle);

impl Drop for Open {
    fn drop(&mut self) {
        self.1.abort();
    }
}

fn open_tunnel(dialer: Arc<dyn Dial>, on_forwarding: OnForwarding) -> Open {
    let (address, task) = open(
        TARGET.parse().unwrap(),
        dialer,
        "remote:local",
        "remote-password",
        on_forwarding,
    )
    .unwrap();
    Open(address, task)
}

#[tokio::test]
async fn answers_keep_only_supported_public_candidates_and_rewrite_to_loopback() {
    let valid = [
        "go_rewrite",
        "ipv6_public",
        "ipv4_mapped",
        "component_two_and_valid",
        "upper_tcp",
        "upper_passive",
        "extensions",
        "google_v6",
        "second_media",
        "session_credentials",
        "media_overrides_session",
        "padded_credentials",
        "v6_embedded_v4",
        "v4_with_zone",
        "exactly_64",
        "tcp_16",
    ];
    for case in inputs()["prepare"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let (answer, offer) = (case["answer"].as_str().unwrap(), case["offer"].as_str().unwrap());
        let prepared = prepare_answer(answer, offer, Arc::new(Never), quiet());
        if !valid.contains(&name) {
            assert!(
                prepared.is_err(),
                "{name}: invalid SDP, credential or target must be refused"
            );
            continue;
        }
        let prepared = prepared.unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(!prepared.tunnels.is_empty());
        let expected_targets: Vec<SocketAddr> = answer
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.strip_prefix("a=candidate:")?.split_whitespace().collect();
                if fields.get(1) != Some(&"1")
                    || !fields.get(2)?.eq_ignore_ascii_case("tcp")
                    || fields.get(7) != Some(&"host")
                    || !fields
                        .windows(2)
                        .any(|pair| pair[0] == "tcptype" && pair[1].eq_ignore_ascii_case("passive"))
                {
                    return None;
                }
                let ip: IpAddr = fields[4].split('%').next().unwrap().parse().unwrap();
                Some(SocketAddr::new(ip.to_canonical(), fields[5].parse().unwrap()))
            })
            .collect();
        assert_eq!(
            prepared.tunnels.iter().map(|tunnel| tunnel.target).collect::<Vec<_>>(),
            expected_targets,
            "{name}: each listener must dial the exact original candidate destination"
        );
        let parsed = parse_sdp(&prepared.sdp).unwrap();
        let (before_user, before_password) = credentials(&parse_sdp(answer).unwrap()).unwrap();
        assert_eq!(credentials(&parsed).unwrap(), (before_user, before_password));
        for tunnel in &prepared.tunnels {
            assert_eq!(tunnel.target.port(), 443);
            assert!(is_public_target(tunnel.target.ip()));
            assert!(tunnel.listener.ip().is_loopback());
            let media = &parsed.media_descriptions[usize::from(tunnel.mline)];
            assert_eq!(media.attribute("mid").flatten(), tunnel.mid.as_deref());
            let fields: Vec<_> = tunnel.candidate.split_whitespace().collect();
            assert!(fields[2].eq_ignore_ascii_case("tcp"));
            assert_eq!(fields[4], tunnel.listener.ip().to_string());
            assert_eq!(fields[5], tunnel.listener.port().to_string());
            assert!(
                fields
                    .windows(2)
                    .any(|pair| pair[0].eq_ignore_ascii_case("tcptype") && pair[1].eq_ignore_ascii_case("passive"))
            );
        }
        let candidates = parsed
            .media_descriptions
            .iter()
            .flat_map(|media| &media.attributes)
            .filter(|a| a.is_ice_candidate())
            .count();
        assert_eq!(
            candidates,
            prepared.tunnels.len(),
            "{name}: no unproxied candidates remain"
        );
        if name == "tcp_16" {
            assert_eq!(prepared.tunnels.len(), 16);
        } else {
            assert_eq!(prepared.tunnels.len(), 1);
        }
    }
}

/// Go's `fragmentedReader`: at most `max` bytes per read.
struct Fragmented {
    data: Vec<u8>,
    max: usize,
}

impl AsyncRead for Fragmented {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let n = self.max.min(buf.remaining()).min(self.data.len());
        let chunk: Vec<u8> = self.data.drain(..n).collect();
        buf.put_slice(&chunk);
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn binding_frames_require_session_credentials_integrity_and_fingerprint() {
    let valid = valid_frame();
    for max in [1, 2, 7, valid.len()] {
        let mut reader = Fragmented {
            data: valid.clone(),
            max,
        };
        assert_eq!(
            read_validated_frame(&mut reader, "remote:local", "remote-password")
                .await
                .unwrap(),
            valid
        );
    }
    for frame in invalid_frames() {
        let mut reader = Fragmented { data: frame, max: 7 };
        assert!(
            read_validated_frame(&mut reader, "remote:local", "remote-password")
                .await
                .is_err()
        );
    }
    for (user, password) in [("local:remote", "remote-password"), ("remote:local", "wrong-password")] {
        let mut reader = Fragmented {
            data: valid.clone(),
            max: 7,
        };
        assert!(read_validated_frame(&mut reader, user, password).await.is_err());
    }
}

#[test]
fn proxy_targets_exclude_special_use_and_private_addresses() {
    // IANA special-purpose ranges, RFC 1918, RFC 6890 and IPv6 transition ranges.
    for (address, public) in [
        ("8.8.8.8", true),
        ("20.42.0.20", true),
        ("2001:4860::1", true),
        ("2606:4700::1", true),
        ("0.0.0.0", false),
        ("10.0.0.1", false),
        ("100.64.0.1", false),
        ("127.0.0.1", false),
        ("169.254.1.1", false),
        ("172.16.0.1", false),
        ("192.0.0.1", false),
        ("192.0.2.1", false),
        ("192.168.1.1", false),
        ("198.18.0.1", false),
        ("198.51.100.1", false),
        ("203.0.113.1", false),
        ("224.0.0.1", false),
        ("255.255.255.255", false),
        ("::", false),
        ("::1", false),
        ("64:ff9b::808:808", false),
        ("2001:db8::1", false),
        ("2002::1", false),
        ("fc00::1", false),
        ("fe80::1", false),
        ("ff02::1", false),
        ("::ffff:127.0.0.1", false),
        ("::ffff:8.8.8.8", true),
    ] {
        let ip: IpAddr = address.parse().unwrap();
        assert_eq!(is_public_target(ip.to_canonical()), public, "{address}");
    }
}

#[test]
fn proxy_settings_decode_supported_schemes_and_credentials() {
    for (url, scheme, port) in [
        ("http://proxy.example", "http", "80"),
        ("https://proxy.example", "https", "443"),
        ("socks5://proxy.example", "socks5", "1080"),
        ("socks5h://proxy.example", "socks5h", "1080"),
    ] {
        assert_eq!(proxy_scheme(url), scheme);
        let dialer = build(url).unwrap().unwrap();
        assert_eq!(dialer.parts().0, "proxy.example");
        assert_eq!(dialer.parts().1, port);
    }
    for url in ["", "  ", "direct"] {
        assert!(build(url).unwrap().is_none());
    }
    for url in ["ftp://proxy.example", "http://proxy.example:bad", "http://[not-an-ip]"] {
        assert!(build(url).is_err(), "{url}");
    }
    let dialer = build("http://user:p%40ss@proxy.example:8080").unwrap().unwrap();
    let (host, port, user) = dialer.parts();
    assert_eq!((host, port), ("proxy.example", "8080"));
    assert_eq!(user.cloned(), Some((b"user".to_vec(), Some(b"p@ss".to_vec()))));
}

/// One scripted SOCKS5 exchange (`runSocks` in the Go harness); returns what the client sent.
async fn socks_server(listener: TcpListener, case: Value) -> Vec<u8> {
    let Ok((mut conn, _)) = listener.accept().await else {
        return vec![];
    };
    let mut sent = Vec::new();
    async fn read(conn: &mut TcpStream, n: usize, sent: &mut Vec<u8>) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; n];
        conn.read_exact(&mut buf).await.ok()?;
        sent.extend_from_slice(&buf);
        Some(buf)
    }
    let method = case["method"].as_u64().unwrap() as u8;
    let reply = unhex(case["reply"].as_str().unwrap());
    let run = async {
        let head = read(&mut conn, 2, &mut sent).await?;
        read(&mut conn, usize::from(head[1]), &mut sent).await?;
        conn.write_all(&[5, method]).await.ok()?;
        if method == 0xff {
            return None;
        }
        if method == 2 {
            let v = read(&mut conn, 2, &mut sent).await?;
            read(&mut conn, usize::from(v[1]), &mut sent).await?;
            let pl = read(&mut conn, 1, &mut sent).await?;
            read(&mut conn, usize::from(pl[0]), &mut sent).await?;
            let ok = case["auth_ok"].as_bool().unwrap();
            conn.write_all(&[1, u8::from(!ok)]).await.ok()?;
            if !ok {
                return None;
            }
        }
        let request = read(&mut conn, 4, &mut sent).await?;
        let address = match request[3] {
            1 => 4,
            4 => 16,
            _ => usize::from(read(&mut conn, 1, &mut sent).await?[0]),
        };
        read(&mut conn, address + 2, &mut sent).await?;
        conn.write_all(&reply).await.ok()?;
        if reply[1] == 0 {
            conn.write_all(b"hello").await.ok()?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Some(())
    };
    let _ = run.await;
    sent
}

/// `runConnect`: the request headers, then the scripted response.
async fn connect_server(listener: TcpListener, response: String) -> Vec<u8> {
    let Ok((mut conn, _)) = listener.accept().await else {
        return vec![];
    };
    let mut sent = Vec::new();
    let mut byte = [0u8; 1];
    while !sent.ends_with(b"\r\n\r\n") && conn.read_exact(&mut byte).await.is_ok() {
        sent.push(byte[0]);
    }
    let _ = conn.write_all(response.as_bytes()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    sent
}

#[tokio::test]
async fn proxy_dialers_obey_socks5_and_connect_contracts() {
    for case in inputs()["dialers"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let server = if case["kind"] == "socks" {
            tokio::spawn(socks_server(listener, case.clone()))
        } else {
            tokio::spawn(connect_server(listener, case["response"].as_str().unwrap().to_owned()))
        };
        let port = host.rsplit(':').next().unwrap().to_owned();
        let url = case["url"]
            .as_str()
            .unwrap()
            .replacen("HOST", &host, 1)
            .replacen("PORT", &port, 1);
        let target: SocketAddr = case["target"].as_str().unwrap().parse().unwrap();
        let dialed = build(&url).unwrap().unwrap().dial(target).await;
        let should_succeed = if case["kind"] == "socks" {
            let method = case["method"].as_u64().unwrap();
            let reply = unhex(case["reply"].as_str().unwrap());
            method != 255 && (method != 2 || case["auth_ok"] == true) && reply[1] == 0
        } else {
            ![
                "http_connect_407",
                "http_connect_garbage",
                "bad_content_length",
                "empty_content_length",
                "two_content_lengths",
                "te_gzip",
                "te_two",
                "chunked_bad_trailer",
                "bad_key_byte",
                "missing_colon",
                "leading_space_header",
                "ctl_in_value",
                "status_201",
                "status_plus",
                "http2_version",
                "eof_in_headers",
                "eof_in_status",
            ]
            .contains(&name)
        };
        if should_succeed {
            let mut conn = dialed.unwrap_or_else(|error| panic!("{name}: {error}"));
            let mut received = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), conn.read_to_end(&mut received))
                .await
                .unwrap()
                .unwrap();
            let expected = if case["kind"] == "socks" {
                "hello"
            } else {
                let response = case["response"].as_str().unwrap();
                response
                    .split_once("\r\n\r\n")
                    .or_else(|| response.split_once("\n\n"))
                    .unwrap()
                    .1
            };
            assert_eq!(
                received,
                expected.as_bytes(),
                "{name}: bytes after handshake belong to tunnel"
            );
        } else {
            assert!(
                dialed.is_err(),
                "{name}: malformed or rejected handshake cannot open a tunnel"
            );
        }
        let sent = server.await.unwrap();
        if case["kind"] == "socks" {
            assert_eq!(sent[0], 5, "SOCKS5 version");
            let methods = usize::from(sent[1]);
            assert!(sent[2..2 + methods].contains(&0));
            if case["method"] == 2 {
                assert_eq!(sent[2 + methods], 1, "RFC 1929 authentication version");
            }
        } else {
            let text = String::from_utf8(sent).unwrap();
            let mut lines = text.lines();
            assert_eq!(lines.next().unwrap(), format!("CONNECT {target} HTTP/1.1"));
            let headers: std::collections::BTreeMap<_, _> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                .collect();
            assert_eq!(headers["host"], target.to_string());
            if url.contains('@') {
                assert!(headers["proxy-authorization"].starts_with("Basic "));
            }
        }
    }
}

/// An `https` proxy: TLS to the proxy (verified, ALPN http/1.1), then `CONNECT` inside.
#[tokio::test]
async fn https_proxy_wraps_connect_in_tls() {
    use btls::asn1::Asn1Time;
    use btls::bn::BigNum;
    use btls::ec::{EcGroup, EcKey};
    use btls::hash::MessageDigest;
    use btls::nid::Nid;
    use btls::pkey::PKey;
    use btls::ssl::{AlpnError, SslAcceptor, SslMethod};
    use btls::x509::extension::SubjectAlternativeName;
    use btls::x509::{X509, X509NameBuilder};

    let key =
        PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap()).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_nid(Nid::COMMONNAME, "proxy.test").unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&BigNum::from_u32(7).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap()).unwrap();
    let san = SubjectAlternativeName::new()
        .ip("127.0.0.1")
        .build(&cert.x509v3_context(None, None))
        .unwrap();
    cert.append_extension(&san).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    let cert = cert.build();

    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&key).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    let (alpn_tx, mut alpn_rx) = mpsc::unbounded_channel();
    acceptor.set_alpn_select_callback(move |_, offered| {
        let _ = alpn_tx.send(offered.to_vec());
        btls::ssl::select_next_proto(b"\x08http/1.1", offered).ok_or(AlpnError::NOACK)
    });
    let acceptor = acceptor.build();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        // The first client does not trust the certificate and aborts its handshake.
        let mut tls = loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
            let mut tls = tokio_btls::SslStream::new(ssl, tcp).unwrap();
            if std::pin::Pin::new(&mut tls).accept().await.is_ok() {
                break tls;
            }
        };
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            tls.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        tls.write_all(b"HTTP/1.1 200 OK\r\n\r\ninside").await.unwrap();
        tls.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        String::from_utf8(request).unwrap()
    });

    let untrusted = build(&format!("https://u:p@{host}"))
        .unwrap()
        .unwrap()
        .dial(TARGET.parse().unwrap())
        .await;
    assert!(
        untrusted
            .err()
            .unwrap()
            .starts_with("HTTPS proxy TLS handshake failed: "),
        "a proxy certificate outside the trusted roots fails"
    );
    let mut conn = build(&format!("https://u:p@{host}"))
        .unwrap()
        .unwrap()
        .trusting(cert)
        .dial(TARGET.parse().unwrap())
        .await
        .unwrap();
    let mut first = [0u8; 6];
    conn.read_exact(&mut first).await.unwrap();
    assert_eq!(&first, b"inside");
    let request = server.await.unwrap();
    assert!(request.starts_with("CONNECT 20.42.0.20:443 HTTP/1.1\r\n"));
    assert!(request.contains("\r\nHost: 20.42.0.20:443\r\n"));
    assert!(request.contains("\r\nProxy-Authorization: Basic dTpw\r\n"));
    let mut offered = None;
    while let Ok(alpn) = alpn_rx.try_recv() {
        offered = Some(alpn);
    }
    assert_eq!(offered.as_deref(), Some(&b"\x08http/1.1"[..]));
}

#[tokio::test]
async fn tunnel_authenticates_before_the_fixed_target_dial() {
    let (dialer, mut dials) = recording(false);
    let (on_forwarding, started) = counter();
    let tunnel = open_tunnel(dialer, on_forwarding);
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    let frame = valid_frame();
    client.write_all(&frame).await.unwrap();
    let (target, far) = tokio::time::timeout(Duration::from_secs(1), dials.recv())
        .await
        .expect("dial after STUN authentication")
        .unwrap();
    assert_eq!(target.to_string(), TARGET);
    let mut far = far.unwrap();
    let mut forwarded = vec![0u8; frame.len()];
    far.read_exact(&mut forwarded).await.unwrap();
    assert_eq!(forwarded, frame);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        *started.lock().unwrap(),
        1,
        "forwarding started once the frame went through"
    );
    far.write_all(b"reply").await.unwrap();
    let mut reply = [0u8; 5];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"reply");
    client.write_all(b"more").await.unwrap();
    let mut more = [0u8; 4];
    far.read_exact(&mut more).await.unwrap();
    assert_eq!(&more, b"more");
    // The listener closed when the first connection claimed the tunnel.
    assert!(TcpStream::connect(tunnel.0).await.is_err());
    // Closing the tunnel closes the spliced client.
    drop(tunnel);
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut rest)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "client closed with the tunnel");
}

/// Go `blockingContextDialer`: the dial never finishes; `canceled` fires when it is dropped.
struct Blocking {
    started: mpsc::UnboundedSender<()>,
    canceled: mpsc::UnboundedSender<()>,
}

struct OnDrop(mpsc::UnboundedSender<()>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

impl Dial for Blocking {
    fn dial(&self, _: SocketAddr) -> BoxFuture<'_, Result<Conn, String>> {
        let _ = self.started.send(());
        let guard = OnDrop(self.canceled.clone());
        Box::pin(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
            Err(String::new())
        })
    }
}

#[tokio::test]
async fn closing_the_tunnel_cancels_the_proxy_dial() {
    let (started_tx, mut started) = mpsc::unbounded_channel();
    let (canceled_tx, mut canceled) = mpsc::unbounded_channel();
    let (on_forwarding, forwarded) = counter();
    let tunnel = open_tunnel(
        Arc::new(Blocking {
            started: started_tx,
            canceled: canceled_tx,
        }),
        on_forwarding,
    );
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    client.write_all(&valid_frame()).await.unwrap();
    let started = tokio::time::timeout(Duration::from_secs(1), started.recv()).await;
    assert_eq!(started, Ok(Some(())), "dial started");
    drop(tunnel);
    let canceled = tokio::time::timeout(Duration::from_secs(1), canceled.recv()).await;
    assert_eq!(canceled, Ok(Some(())), "close cancelled the dial");
    assert_eq!(*forwarded.lock().unwrap(), 0);
}

#[tokio::test]
async fn proxy_failure_does_not_fall_back() {
    let (dialer, mut dials) = recording(true);
    let (on_forwarding, forwarded) = counter();
    let tunnel = open_tunnel(dialer, on_forwarding);
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    client.write_all(&valid_frame()).await.unwrap();
    let (target, conn) = tokio::time::timeout(Duration::from_secs(1), dials.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((target.to_string().as_str(), conn.is_none()), (TARGET, true));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        TcpStream::connect(tunnel.0).await.is_err(),
        "candidate listener closed after the proxy failure"
    );
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut rest)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "client closed");
    assert_eq!(*forwarded.lock().unwrap(), 0);
}

/// Go `closedUpstreamDialer`: the proxied connection is already closed.
struct Closed;

impl Dial for Closed {
    fn dial(&self, _: SocketAddr) -> BoxFuture<'_, Result<Conn, String>> {
        Box::pin(async {
            let (near, far) = tokio::io::duplex(64);
            drop(far);
            Ok(Box::new(near) as Conn)
        })
    }
}

#[tokio::test]
async fn write_failure_does_not_log_forwarding_start() {
    let (on_forwarding, forwarded) = counter();
    let tunnel = open_tunnel(Arc::new(Closed), on_forwarding);
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    client.write_all(&valid_frame()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*forwarded.lock().unwrap(), 0);
}

#[tokio::test]
async fn unauthenticated_connections_never_dial() {
    let (dialer, mut dials) = recording(false);
    let (on_forwarding, forwarded) = counter();
    let tunnel = open_tunnel(dialer, on_forwarding);
    for (index, frame) in invalid_frames().into_iter().enumerate() {
        let mut client = TcpStream::connect(tunnel.0).await.unwrap();
        client.write_all(&frame).await.unwrap();
        client.shutdown().await.unwrap();
        let mut rest = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut rest)).await;
        assert!(
            matches!(read, Ok(Ok(0)) | Ok(Err(_))),
            "invalid frame {index}: rejected connections close"
        );
    }
    // A frame for another session (Go: `attacker:local`).
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    let mut frame = valid_frame();
    let at = frame.windows(12).position(|w| w == b"remote:local").unwrap();
    frame[at..at + 6].copy_from_slice(b"attack");
    client.write_all(&frame).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), dials.recv())
            .await
            .is_err(),
        "no dial for an unauthenticated connection"
    );
    assert_eq!(*forwarded.lock().unwrap(), 0);
}

#[tokio::test]
async fn at_most_four_connections_wait_for_authentication() {
    let (dialer, mut dials) = recording(false);
    let tunnel = open_tunnel(dialer, quiet());
    let mut idle = Vec::new();
    for _ in 0..4 {
        idle.push(TcpStream::connect(tunnel.0).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut excess = TcpStream::connect(tunnel.0).await.unwrap();
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(1), excess.read(&mut buf)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "the fifth is closed at once");
    // A waiting connection that gives up frees its slot for the real one.
    drop(idle.pop());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut client = TcpStream::connect(tunnel.0).await.unwrap();
    client.write_all(&valid_frame()).await.unwrap();
    let (target, _) = tokio::time::timeout(Duration::from_secs(1), dials.recv())
        .await
        .expect("the freed slot accepted the session's connection")
        .unwrap();
    assert_eq!(target.to_string(), TARGET);
}

#[tokio::test]
async fn tunnel_targets_and_credentials_are_checked() {
    for (target, user, password, want) in [
        (
            "10.0.0.1:443",
            "remote:local",
            "pw",
            "Codex live TCP proxy target is not allowed",
        ),
        (
            "20.42.0.20:8443",
            "remote:local",
            "pw",
            "Codex live TCP proxy target is not allowed",
        ),
        (
            "20.42.0.20:443",
            " ",
            "pw",
            "Codex live TCP proxy tunnel configuration is incomplete",
        ),
        (
            "20.42.0.20:443",
            "remote:local",
            "",
            "Codex live TCP proxy tunnel configuration is incomplete",
        ),
    ] {
        let opened = open(target.parse().unwrap(), Arc::new(Never), user, password, quiet());
        assert_eq!(opened.err().as_deref(), Some(want), "{target} {user:?} {password:?}");
    }
}
