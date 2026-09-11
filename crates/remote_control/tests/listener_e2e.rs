//! End-to-end smoke test for the R-2 Remote Control listener.
//!
//! Drives the full handshake — TCP connect → TLS 1.3 (with a custom
//! verifier pinning the server's self-signed cert by SHA-256) →
//! WebSocket upgrade → HMAC-SHA256 challenge response → JSON-RPC
//! request/response — against an in-process listener.
//!
//! The test is the load-bearing acceptance gate for R-2 (per
//! `docs/plans/2026-05-15-remote-control-R2.md` § G).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use chrono::Utc;
use futures::future::BoxFuture;
use futures::{SinkExt as _, StreamExt as _};
use hmac::{Hmac, Mac};
use remote_control::auth::HMAC_DOMAIN_TAG;
use remote_control::cert::ServerCert;
use remote_control::dispatch::{
    ConnectionDispatcher, JsonRpcRequest, JsonRpcResponse, MinimalDispatcher, PendingResponse,
    RemoteDispatcher,
};
use remote_control::listener::{self, ListenerConfig};
use remote_control::proxy::{NotificationReceiver, notification_channel};
use remote_control::{AuthorizedClient, RemoteControlSettings};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest as _, Sha256};
use tokio_tungstenite::tungstenite::Message;

/// Custom rustls verifier that accepts exactly one cert (by SHA-256 of
/// its DER bytes) and rejects everything else — the Android client's
/// `OkHttpClient + CertificatePinner` equivalent in pure rustls.
#[derive(Debug)]
struct FingerprintPinningVerifier {
    expected_fingerprint: [u8; 32],
}

impl ServerCertVerifier for FingerprintPinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let got: [u8; 32] = hasher.finalize().into();
        if got == self.expected_fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "fingerprint mismatch: expected {:?}, got {:?}",
                hex::encode(self.expected_fingerprint),
                hex::encode(got)
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ED25519,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

fn build_client_tls_config(fingerprint: [u8; 32]) -> Arc<ClientConfig> {
    // Install the same crypto provider rustls uses for the server side
    // (`aws_lc_rs`). Installing twice is fine — `install_default` returns
    // an error which we ignore.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();

    let verifier = Arc::new(FingerprintPinningVerifier {
        expected_fingerprint: fingerprint,
    });
    let config = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Arc::new(config)
}

fn make_authorized_client(name: &str) -> AuthorizedClient {
    // Mix the name into the secret so two named-different clients in the
    // same test get distinct secrets. Deterministic for reproducibility.
    let mut secret = [0u8; 32];
    for (i, b) in secret.iter_mut().enumerate() {
        let name_byte = name
            .as_bytes()
            .get(i % name.len().max(1))
            .copied()
            .unwrap_or(0);
        *b = (i as u8).wrapping_mul(7).wrapping_add(name_byte);
    }
    AuthorizedClient {
        name: name.into(),
        secret_base64: base64::engine::general_purpose::STANDARD.encode(secret),
        created_at: Utc::now(),
    }
}

fn make_server_cert() -> ServerCert {
    // Lean on the real cert module's generator (synchronous part). We
    // can't go through `load_or_generate` here without an `fs::Fs`, so
    // we mint inline using the same rcgen path the production code uses.
    // The fingerprint we return matches what the listener will see.
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into().expect("dns san")),
        SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
    ];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "spk-test");
    params.distinguished_name = dn;

    let key_pair = KeyPair::generate().expect("keypair");
    let cert = params.self_signed(&key_pair).expect("self-sign");
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();
    let mut hasher = Sha256::new();
    hasher.update(&cert_der);
    let fingerprint: [u8; 32] = hasher.finalize().into();
    ServerCert {
        cert_der,
        key_der,
        fingerprint_sha256: fingerprint,
    }
}

fn compute_response(secret_base64: &str, challenge: &[u8; 16]) -> [u8; 32] {
    let secret = base64::engine::general_purpose::STANDARD
        .decode(secret_base64)
        .expect("decode");
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&secret).expect("hmac");
    mac.update(HMAC_DOMAIN_TAG);
    mac.update(challenge);
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_handshake_and_minimal_dispatcher_round_trip() -> Result<()> {
    let client = make_authorized_client("Phone");
    let mut settings = RemoteControlSettings::default();
    settings.clients.push(client.clone());

    let cert = make_server_cert();
    let fingerprint = cert.fingerprint_sha256;
    let (clients_tx, clients_rx) = tokio::sync::watch::channel(settings.clients.clone());
    let dispatcher: Arc<dyn remote_control::dispatch::RemoteDispatcher> = MinimalDispatcher::new();

    let cfg = ListenerConfig {
        bind_addr: ([127, 0, 0, 1], 0).into(),
        cert,
        clients_rx,
        dispatcher,
        idle_timeout: listener::DEFAULT_IDLE_READ_TIMEOUT,
    };
    let handle = listener::start_listener(cfg).await?;
    let addr = handle.bound_addr();

    // ---------- client side ----------
    let tls_config = build_client_tls_config(fingerprint);
    let tls_connector = tokio_tungstenite::Connector::Rustls(tls_config);
    let url = format!("wss://127.0.0.1:{}/", addr.port());

    let request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        url.as_str(),
    )?;
    let (mut ws, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(tls_connector))
            .await?;

    // 1. Read challenge.
    let challenge_frame = ws
        .next()
        .await
        .expect("must receive challenge")
        .expect("ws ok");
    let challenge_text = match challenge_frame {
        Message::Text(t) => t,
        other => panic!("expected text challenge, got {other:?}"),
    };
    let parsed: serde_json::Value = serde_json::from_str(challenge_text.as_ref())?;
    assert_eq!(parsed["type"], "challenge");
    let challenge_hex = parsed["challenge"].as_str().expect("challenge hex");
    let challenge_bytes = hex::decode(challenge_hex)?;
    let mut challenge = [0u8; 16];
    challenge.copy_from_slice(&challenge_bytes);

    // 2. Compute response and send.
    let response = compute_response(&client.secret_base64, &challenge);
    let response_frame = serde_json::json!({
        "type": "response",
        "response": hex::encode(response),
    });
    ws.send(Message::Text(response_frame.to_string().into()))
        .await?;

    // 3. Read welcome.
    let welcome_frame = ws
        .next()
        .await
        .expect("must receive welcome")
        .expect("ws ok");
    let welcome_text = match welcome_frame {
        Message::Text(t) => t,
        other => panic!("expected text welcome, got {other:?}"),
    };
    let welcome: serde_json::Value = serde_json::from_str(welcome_text.as_ref())?;
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["client"], "Phone");

    // 4. JSON-RPC ping.
    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":1,"method":"remote.editor.ping"}"#.into(),
    ))
    .await?;
    let ping_reply = ws
        .next()
        .await
        .expect("must receive ping reply")
        .expect("ws ok");
    let ping_text = match ping_reply {
        Message::Text(t) => t,
        other => panic!("expected text ping reply, got {other:?}"),
    };
    let ping_parsed: serde_json::Value = serde_json::from_str(ping_text.as_ref())?;
    assert_eq!(ping_parsed["id"], 1);
    assert_eq!(ping_parsed["result"]["pong"], true);

    // 5. JSON-RPC capabilities.
    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":2,"method":"remote.editor.capabilities"}"#.into(),
    ))
    .await?;
    let caps_reply = ws
        .next()
        .await
        .expect("must receive capabilities reply")
        .expect("ws ok");
    let caps_text = match caps_reply {
        Message::Text(t) => t,
        other => panic!("expected text caps reply, got {other:?}"),
    };
    let caps_parsed: serde_json::Value = serde_json::from_str(caps_text.as_ref())?;
    assert_eq!(caps_parsed["id"], 2);
    assert_eq!(caps_parsed["result"]["protocol_version"], 1);
    assert_eq!(caps_parsed["result"]["server_software"], "sawe");

    // 6. JSON-RPC unknown method → -32601.
    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":3,"method":"remote.unknown.method"}"#.into(),
    ))
    .await?;
    let unknown_reply = ws
        .next()
        .await
        .expect("must receive unknown-method reply")
        .expect("ws ok");
    let unknown_text = match unknown_reply {
        Message::Text(t) => t,
        other => panic!("expected text reply, got {other:?}"),
    };
    let unknown_parsed: serde_json::Value = serde_json::from_str(unknown_text.as_ref())?;
    assert_eq!(unknown_parsed["id"], 3);
    assert_eq!(unknown_parsed["error"]["code"], -32601);

    // Close cleanly.
    ws.close(None).await?;

    // 7. Tear down: drop the handle, give the runtime a tick to actually
    // close the socket, then assert reconnect fails (ECONNREFUSED).
    drop(handle);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let reconnect = tokio::net::TcpStream::connect(addr).await;
    assert!(
        reconnect.is_err(),
        "expected reconnect to fail after listener drop, got {reconnect:?}",
    );

    // Silence the unused `clients_tx` — the broadcast path is exercised
    // separately in the store unit tests.
    drop(clients_tx);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_rejects_unauthorized_client() -> Result<()> {
    // Server knows client "Phone"; we'll authenticate as "Rogue" (wrong
    // secret). The connection MUST close with WS code 1008.
    let phone = make_authorized_client("Phone");
    let rogue = make_authorized_client("Rogue");
    let mut settings = RemoteControlSettings::default();
    settings.clients.push(phone.clone());

    let cert = make_server_cert();
    let fingerprint = cert.fingerprint_sha256;
    let (_clients_tx, clients_rx) = tokio::sync::watch::channel(settings.clients.clone());
    let dispatcher: Arc<dyn remote_control::dispatch::RemoteDispatcher> = MinimalDispatcher::new();

    let cfg = ListenerConfig {
        bind_addr: ([127, 0, 0, 1], 0).into(),
        cert,
        clients_rx,
        dispatcher,
        idle_timeout: listener::DEFAULT_IDLE_READ_TIMEOUT,
    };
    let handle = listener::start_listener(cfg).await?;
    let addr = handle.bound_addr();

    let tls_config = build_client_tls_config(fingerprint);
    let tls_connector = tokio_tungstenite::Connector::Rustls(tls_config);
    let url = format!("wss://127.0.0.1:{}/", addr.port());
    let request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        url.as_str(),
    )?;
    let (mut ws, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(tls_connector))
            .await?;

    // Read challenge but reply with the wrong secret.
    let challenge_frame = ws
        .next()
        .await
        .expect("must receive challenge")
        .expect("ws ok");
    let challenge_text = match challenge_frame {
        Message::Text(t) => t,
        other => panic!("expected text challenge, got {other:?}"),
    };
    let parsed: serde_json::Value = serde_json::from_str(challenge_text.as_ref())?;
    let challenge_hex = parsed["challenge"].as_str().expect("hex");
    let challenge_bytes = hex::decode(challenge_hex)?;
    let mut challenge = [0u8; 16];
    challenge.copy_from_slice(&challenge_bytes);

    // Use rogue's secret — server has no entry for it, must close.
    let response = compute_response(&rogue.secret_base64, &challenge);
    let response_frame = serde_json::json!({
        "type": "response",
        "response": hex::encode(response),
    });
    ws.send(Message::Text(response_frame.to_string().into()))
        .await?;

    // Next message should be a Close frame; subsequent reads return None.
    let next = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("expected close within 2s");
    match next {
        Some(Ok(Message::Close(Some(close)))) => {
            assert_eq!(
                u16::from(close.code),
                1008,
                "expected WS policy code 1008, got {:?}",
                close.code
            );
            assert!(
                close.reason.contains("unauthorized"),
                "reason: {:?}",
                close.reason
            );
        }
        other => panic!("expected Close(1008), got {other:?}"),
    }

    drop(handle);
    Ok(())
}

/// Order in which requests were committed upstream, as recorded by
/// [`TestConnection::begin_dispatch`].
type CommitOrder = Arc<std::sync::Mutex<Vec<i64>>>;

/// Dispatcher stub with the knobs the concurrency / ordering / liveness
/// tests need: a method that takes a configurable while to answer, replies
/// that deliberately complete out of order, and an optional notification
/// stream that keeps producing outbound traffic.
struct TestDispatcher {
    slow_delay: Duration,
    /// Size of the payload `remote.editor.bulk` answers with. Big enough
    /// and the writer blocks inside `sink.send` until the client drains.
    bulk_bytes: usize,
    /// Make replies complete in REVERSE request-id order, so a test that
    /// asserts commit order can't pass just because the responses happened
    /// to come back in order.
    reverse_replies: bool,
    notify_every: Option<Duration>,
    commit_order: CommitOrder,
}

impl TestDispatcher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            slow_delay: Duration::from_secs(2),
            bulk_bytes: 0,
            reverse_replies: false,
            notify_every: None,
            commit_order: CommitOrder::default(),
        })
    }

    fn notifying(interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            slow_delay: Duration::ZERO,
            bulk_bytes: 0,
            reverse_replies: false,
            notify_every: Some(interval),
            commit_order: CommitOrder::default(),
        })
    }

    fn recording_order() -> Arc<Self> {
        Arc::new(Self {
            slow_delay: Duration::ZERO,
            bulk_bytes: 0,
            reverse_replies: true,
            notify_every: None,
            commit_order: CommitOrder::default(),
        })
    }

    fn bulky(bulk_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            slow_delay: Duration::ZERO,
            bulk_bytes,
            reverse_replies: false,
            notify_every: None,
            commit_order: CommitOrder::default(),
        })
    }
}

impl RemoteDispatcher for TestDispatcher {
    fn open_connection(&self) -> BoxFuture<'static, Result<Box<dyn ConnectionDispatcher>>> {
        let slow_delay = self.slow_delay;
        let bulk_bytes = self.bulk_bytes;
        let reverse_replies = self.reverse_replies;
        let notify_every = self.notify_every;
        let commit_order = self.commit_order.clone();
        Box::pin(async move {
            let notifications = notify_every.map(|interval| {
                let (sender, receiver) = notification_channel();
                tokio::spawn(async move {
                    // Bounded so a finished test can't leave a task
                    // spinning for the rest of the binary's life.
                    for _ in 0..500 {
                        tokio::time::sleep(interval).await;
                        sender.send(serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "editor/notification",
                            "params": {
                                "kind": "agent_session_title_changed",
                                "payload": { "session_id": "s" },
                            },
                        }));
                    }
                });
                receiver
            });
            let connection: Box<dyn ConnectionDispatcher> = Box::new(TestConnection {
                slow_delay,
                bulk_bytes,
                reverse_replies,
                notifications,
                commit_order,
                upstream: tokio::sync::Mutex::new(()),
            });
            Ok(connection)
        })
    }
}

struct TestConnection {
    slow_delay: Duration,
    bulk_bytes: usize,
    reverse_replies: bool,
    notifications: Option<NotificationReceiver>,
    commit_order: CommitOrder,
    /// Stand-in for `UnixMcpProxy`'s `write_half` mutex — the thing that
    /// actually decides which request the editor sees first.
    upstream: tokio::sync::Mutex<()>,
}

impl ConnectionDispatcher for TestConnection {
    // `async_yields_async` is exactly the shape this method is for: the
    // outer future is the commit (awaited by the reader in wire order) and
    // the value it yields is the reply-wait (spawned). Collapsing them is
    // the bug the split exists to prevent.
    #[allow(clippy::async_yields_async)]
    fn begin_dispatch<'a>(
        &'a self,
        _client_name: &'a str,
        request: JsonRpcRequest,
    ) -> BoxFuture<'a, PendingResponse> {
        Box::pin(async move {
            // Model the real `begin_call`: take the shared upstream lock and
            // yield once (a `write_all` on a Unix socket does). Whatever
            // order this runs in IS the order the editor would have seen the
            // requests, which is why the recording lives here and not in the
            // reply phase.
            {
                let _upstream = self.upstream.lock().await;
                tokio::task::yield_now().await;
                if let Some(id) = request.id.as_i64() {
                    self.commit_order
                        .lock()
                        .expect("commit-order mutex")
                        .push(id);
                }
            }

            let bulk =
                (request.method == "remote.editor.bulk").then(|| "x".repeat(self.bulk_bytes));
            let mut reply_delay = Duration::ZERO;
            if request.method == "remote.editor.slow" {
                reply_delay += self.slow_delay;
            }
            if self.reverse_replies {
                let id = request.id.as_i64().unwrap_or(0).clamp(0, 64);
                reply_delay += Duration::from_millis((64 - id) as u64 * 10);
            }
            Box::pin(async move {
                if !reply_delay.is_zero() {
                    tokio::time::sleep(reply_delay).await;
                }
                match bulk {
                    Some(payload) => JsonRpcResponse::ok(
                        request.id,
                        serde_json::json!({ "method": request.method, "bulk": payload }),
                    ),
                    None => JsonRpcResponse::ok(
                        request.id,
                        serde_json::json!({ "method": request.method, "ok": true }),
                    ),
                }
            }) as PendingResponse
        })
    }

    fn take_notifications(&mut self) -> Option<NotificationReceiver> {
        self.notifications.take()
    }
}

type TestWebSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Bring up a listener on an ephemeral port and complete the full client
/// handshake against it, returning the authenticated socket alongside the
/// pieces a test needs to drive revocation / shutdown.
async fn start_and_authenticate(
    dispatcher: Arc<dyn RemoteDispatcher>,
    idle_timeout: Duration,
) -> Result<(
    listener::ListenerHandle,
    tokio::sync::watch::Sender<Vec<AuthorizedClient>>,
    TestWebSocket,
)> {
    let client = make_authorized_client("Phone");
    let cert = make_server_cert();
    let fingerprint = cert.fingerprint_sha256;
    let (clients_tx, clients_rx) = tokio::sync::watch::channel(vec![client.clone()]);

    let cfg = ListenerConfig {
        bind_addr: ([127, 0, 0, 1], 0).into(),
        cert,
        clients_rx,
        dispatcher,
        idle_timeout,
    };
    let handle = listener::start_listener(cfg).await?;
    let addr = handle.bound_addr();

    let tls_config = build_client_tls_config(fingerprint);
    let tls_connector = tokio_tungstenite::Connector::Rustls(tls_config);
    let url = format!("wss://127.0.0.1:{}/", addr.port());
    let request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        url.as_str(),
    )?;
    let (mut ws, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(tls_connector))
            .await?;

    let challenge_frame = ws.next().await.expect("challenge")?;
    let challenge_text = match challenge_frame {
        Message::Text(text) => text,
        other => panic!("expected text challenge, got {other:?}"),
    };
    let parsed: serde_json::Value = serde_json::from_str(challenge_text.as_ref())?;
    let challenge_bytes = hex::decode(parsed["challenge"].as_str().expect("challenge hex"))?;
    let mut challenge = [0u8; 16];
    challenge.copy_from_slice(&challenge_bytes);
    let response = compute_response(&client.secret_base64, &challenge);
    ws.send(Message::Text(
        serde_json::json!({ "type": "response", "response": hex::encode(response) })
            .to_string()
            .into(),
    ))
    .await?;
    let welcome = ws.next().await.expect("welcome")?;
    match welcome {
        Message::Text(text) => {
            let value: serde_json::Value = serde_json::from_str(text.as_ref())?;
            assert_eq!(value["type"], "welcome");
        }
        other => panic!("expected welcome, got {other:?}"),
    }
    Ok((handle, clients_tx, ws))
}

/// Read frames until one carries a JSON-RPC `id`, and return it.
/// Notifications (which have no `id`) are skipped.
async fn next_response(ws: &mut TestWebSocket) -> Result<serde_json::Value> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for a response frame"))?
            .ok_or_else(|| anyhow::anyhow!("socket closed while waiting for a response"))??;
        let Message::Text(text) = frame else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(text.as_ref())?;
        if value.get("id").is_some_and(|id| !id.is_null()) {
            return Ok(value);
        }
    }
}

/// Read frames until a Close arrives, returning `(code, reason)`.
async fn next_close(ws: &mut TestWebSocket, within: Duration) -> Result<(u16, String)> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let frame = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for a close frame"))?;
        match frame {
            Some(Ok(Message::Close(Some(close)))) => {
                return Ok((u16::from(close.code), close.reason.to_string()));
            }
            Some(Ok(Message::Close(None))) => return Ok((1005, String::new())),
            Some(Ok(_)) => continue,
            Some(Err(err)) => return Err(err.into()),
            None => anyhow::bail!("socket ended without a close frame"),
        }
    }
}

/// N-50: the request loop used to dispatch inline, so one slow RPC held
/// the whole connection — no pongs, no notifications, no other RPCs —
/// for as long as it ran (up to the 30 s proxy call timeout). A phone
/// could not tell "the server is busy with my own big response" from a
/// dead socket. Requests now run on their own tasks and answer out of
/// order, which JSON-RPC allows (the client matches on `id`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_rpc_does_not_block_a_concurrent_one() -> Result<()> {
    let (handle, _clients_tx, mut ws) =
        start_and_authenticate(TestDispatcher::new(), listener::DEFAULT_IDLE_READ_TIMEOUT).await?;

    let started = std::time::Instant::now();
    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":1,"method":"remote.editor.slow"}"#.into(),
    ))
    .await?;
    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":2,"method":"remote.editor.ping"}"#.into(),
    ))
    .await?;

    let first = next_response(&mut ws).await?;
    assert_eq!(
        first["id"], 2,
        "the fast RPC must answer while the slow one is still running"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "fast reply took {:?} — it was queued behind the 2 s call",
        started.elapsed(),
    );

    let second = next_response(&mut ws).await?;
    assert_eq!(second["id"], 1);

    drop(handle);
    Ok(())
}

/// H1: requests must reach the editor in the order they arrived on the
/// wire. The reader dispatches concurrently now, so the ONLY thing keeping
/// that true is that the commit half of a dispatch runs inline on the
/// reader; spawning the whole call instead lets the tasks race for the
/// upstream write mutex — and tokio's LIFO slot makes the *newest* task
/// win, so two frames delivered in one TCP segment invert essentially
/// every time.
///
/// The client depends on this: `QueueController` releases each queued send
/// as soon as the previous frame reaches the transport, so flushing an
/// offline backlog puts every message in flight at once. Inverting them
/// reorders the transcript permanently, and nothing heals it.
///
/// The stub records arrival inside `begin_dispatch` (under a shared lock,
/// with a yield, exactly like the real `write_all`) and answers in reverse
/// id order, so this cannot pass by accident.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_reach_the_dispatcher_in_wire_order() -> Result<()> {
    let dispatcher = TestDispatcher::recording_order();
    let commit_order = dispatcher.commit_order.clone();
    let (handle, _clients_tx, mut ws) =
        start_and_authenticate(dispatcher, listener::DEFAULT_IDLE_READ_TIMEOUT).await?;

    const REQUESTS: i64 = 12;
    // One `send` per frame, no reads in between: the frames land in the
    // server's receive buffer back-to-back, which is the shape that
    // reproduces the inversion.
    for id in 1..=REQUESTS {
        ws.send(Message::Text(
            format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"remote.editor.ping"}}"#).into(),
        ))
        .await?;
    }

    let mut answered = Vec::new();
    for _ in 0..REQUESTS {
        answered.push(next_response(&mut ws).await?["id"].as_i64().expect("id"));
    }
    answered.sort_unstable();
    assert_eq!(
        answered,
        (1..=REQUESTS).collect::<Vec<_>>(),
        "every request must be answered exactly once"
    );

    let committed = commit_order.lock().expect("commit-order mutex").clone();
    assert_eq!(
        committed,
        (1..=REQUESTS).collect::<Vec<_>>(),
        "requests must be committed upstream in wire order, got {committed:?}"
    );

    drop(handle);
    Ok(())
}

/// N-08: an inbound frame over the 1 MiB cap makes tungstenite abort the
/// read. The socket used to be torn down with no close frame at all, so
/// the phone read it as a network blip and retried the same poison frame
/// on a ~1 s loop. 1009 lets it classify the failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_frame_is_closed_with_1009() -> Result<()> {
    let (handle, _clients_tx, mut ws) =
        start_and_authenticate(TestDispatcher::new(), listener::DEFAULT_IDLE_READ_TIMEOUT).await?;

    let padding = "x".repeat(1024 * 1024 + 4096);
    let oversize =
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"remote.editor.ping","params":"{padding}"}}"#);
    ws.send(Message::Text(oversize.into())).await?;

    let (code, reason) = next_close(&mut ws, Duration::from_secs(10)).await?;
    assert_eq!(
        code, 1009,
        "expected WS 1009 (message too big), reason={reason:?}"
    );
    assert!(reason.contains("too big"), "reason: {reason:?}");

    drop(handle);
    Ok(())
}

/// N-51: turning Remote Control off only dropped the accept loop. Live
/// connections kept working — a paired phone could still send messages
/// and authorise tool calls against a server that considered itself off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_listener_closes_live_connections() -> Result<()> {
    let (handle, _clients_tx, mut ws) =
        start_and_authenticate(TestDispatcher::new(), listener::DEFAULT_IDLE_READ_TIMEOUT).await?;

    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":1,"method":"remote.editor.ping"}"#.into(),
    ))
    .await?;
    assert_eq!(next_response(&mut ws).await?["id"], 1);

    drop(handle);

    let (code, reason) = next_close(&mut ws, Duration::from_secs(10)).await?;
    assert_eq!(code, 1001);
    assert_eq!(reason, "server shutting down");
    Ok(())
}

/// N-54: a revoked client was kicked with "evicted by new connection",
/// which reads as "you connected twice" — the opposite of what happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_client_is_closed_with_its_own_reason() -> Result<()> {
    let (handle, clients_tx, mut ws) =
        start_and_authenticate(TestDispatcher::new(), listener::DEFAULT_IDLE_READ_TIMEOUT).await?;

    clients_tx
        .send(Vec::new())
        .expect("listener holds a receiver");

    let (code, reason) = next_close(&mut ws, Duration::from_secs(10)).await?;
    assert_eq!(code, 1001);
    assert_eq!(reason, "authorization revoked");

    drop(handle);
    Ok(())
}

/// M1: a close must reach the client even when the writer is already
/// blocked pushing a large frame — which, on a slow link, is most of the
/// time.
///
/// Before, `close_rx` was only polled between queue items, so a close was
/// invisible to the writer for the whole duration of the frame in flight;
/// the reader gave up after its grace and aborted the writer, and the
/// phone got a bare TCP reset instead of "authorization revoked". The
/// close reasons the rest of this work added (1009, "server shutting
/// down", "idle timeout") were unreachable in exactly the situation they
/// were most needed.
///
/// The client stops reading so the 8 MB response wedges the writer, the
/// server revokes it, and only then does the client drain — later than the
/// old grace, sooner than the current close-write budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_preempts_a_frame_already_in_flight() -> Result<()> {
    let (handle, clients_tx, mut ws) = start_and_authenticate(
        TestDispatcher::bulky(8 * 1024 * 1024),
        listener::DEFAULT_IDLE_READ_TIMEOUT,
    )
    .await?;

    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":1,"method":"remote.editor.bulk"}"#.into(),
    ))
    .await?;
    // Let the response reach the writer and fill the socket buffers.
    tokio::time::sleep(Duration::from_millis(500)).await;

    clients_tx
        .send(Vec::new())
        .expect("listener holds a receiver");

    // Stay silent past the point where the reader used to abort the writer,
    // then start draining.
    tokio::time::sleep(Duration::from_secs(7)).await;

    let (code, reason) = next_close(&mut ws, Duration::from_secs(20)).await?;
    assert_eq!(code, 1001);
    assert_eq!(reason, "authorization revoked");

    drop(handle);
    Ok(())
}

/// N-53: the idle timer was re-armed by every loop iteration, and
/// delivering a notification is a loop iteration — so a phone that had
/// stopped receiving was never detected as long as the editor kept
/// streaming at it. The timer now measures INBOUND silence only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_timeout_is_not_rearmed_by_outbound_notifications() -> Result<()> {
    let (handle, _clients_tx, mut ws) = start_and_authenticate(
        TestDispatcher::notifying(Duration::from_millis(100)),
        Duration::from_secs(1),
    )
    .await?;

    // One request opens the dispatcher connection, which is what starts
    // the notification pump.
    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":1,"method":"remote.editor.ping"}"#.into(),
    ))
    .await?;
    assert_eq!(next_response(&mut ws).await?["id"], 1);

    // Keep draining (so the socket never backs up) but send nothing.
    let (code, reason) = next_close(&mut ws, Duration::from_secs(10)).await?;
    assert_eq!(code, 1001);
    assert_eq!(reason, "idle timeout");

    drop(handle);
    Ok(())
}
