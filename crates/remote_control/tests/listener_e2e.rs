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
use futures::{SinkExt as _, StreamExt as _};
use hmac::{Hmac, Mac};
use remote_control::auth::HMAC_DOMAIN_TAG;
use remote_control::cert::ServerCert;
use remote_control::dispatch::MinimalDispatcher;
use remote_control::listener::{self, ListenerConfig};
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
