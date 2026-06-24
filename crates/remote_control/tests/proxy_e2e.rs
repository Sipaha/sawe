//! Proxy end-to-end: WS client → Remote Control listener → ProxyDispatcher
//! → embedded `editor_mcp` Unix socket → real `editor.capabilities` tool.
//!
//! This is the load-bearing acceptance gate for R-4. The R-2 listener_e2e
//! test uses `MinimalDispatcher` (stub) and asserts the wire works; this
//! test asserts the wire is now wired to the actual MCP tool catalogue.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use gpui::{AppContext as _, Entity, TestAppContext};
use hmac::{Hmac, Mac};
use remote_control::RemoteControlStore;
use remote_control::auth::HMAC_DOMAIN_TAG;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest as _, Sha256};
use tokio_tungstenite::tungstenite::Message;

/// Custom rustls verifier mirroring the R-2 test — pins a single SHA-256
/// fingerprint. Used here to talk to the listener's self-signed cert
/// without a real CA.
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
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
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
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
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

fn compute_response(secret_base64: &str, challenge: &[u8; 16]) -> [u8; 32] {
    let secret = base64::engine::general_purpose::STANDARD
        .decode(secret_base64)
        .expect("decode secret");
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&secret).expect("hmac key");
    mac.update(HMAC_DOMAIN_TAG);
    mac.update(challenge);
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    out
}

async fn poll_until<T, F>(
    cx: &mut TestAppContext,
    store: &Entity<RemoteControlStore>,
    mut predicate: F,
) -> Option<T>
where
    F: FnMut(&RemoteControlStore) -> Option<T>,
{
    for _ in 0..200 {
        cx.run_until_parked();
        let snapshot = store.read_with(cx, |store, _| predicate(store));
        if snapshot.is_some() {
            return snapshot;
        }
        cx.background_executor
            .timer(Duration::from_millis(25))
            .await;
    }
    None
}

#[gpui::test]
async fn end_to_end_proxy_round_trip(cx: &mut TestAppContext) {
    run_proxy_round_trip(cx).await.expect("proxy round-trip");
}

async fn run_proxy_round_trip(cx: &mut TestAppContext) -> Result<()> {
    cx.executor().allow_parking();

    // ---- 1. Pin editor_mcp to a tempdir socket. ----
    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    cx.update(|cx| editor_mcp::init(cx));

    // ---- 2. Boot the embedded MCP server. ----
    let start_result = cx.update(|cx| editor_mcp::start_server(cx));
    assert!(
        start_result.is_ok(),
        "start_server: {:?}",
        start_result.err()
    );

    let socket_path = runtime_dir.path().join("mcp.sock");
    let mut waited = Duration::ZERO;
    while !socket_path.exists() && waited < Duration::from_secs(10) {
        cx.executor().timer(Duration::from_millis(100)).await;
        waited += Duration::from_millis(100);
    }
    assert!(socket_path.exists(), "mcp.sock did not appear");

    // ---- 3. Boot the Remote Control listener (production wiring). ----
    cx.update(gpui_tokio::init);
    let fs: Arc<dyn fs::Fs> = fs::FakeFs::new(cx.background_executor.clone());
    let store = cx.new(|cx| RemoteControlStore::new_with_fs(fs, cx));

    let secret = store.update(cx, |store, cx| {
        store.set_address(Some("127.0.0.1".into()), cx);
        store.set_port(0, cx);
        let client = store.add_client("Test".into(), cx).expect("add client");
        client.secret_base64
    });
    store.update(cx, |store, cx| store.set_enabled(true, cx));

    let bound_addr = poll_until(cx, &store, |s| s.bound_addr())
        .await
        .expect("listener must bind");
    let fingerprint = store
        .read_with(cx, |s, _| s.cert_fingerprint())
        .expect("cert fingerprint");

    // ---- 4. Run WebSocket client work inside the tokio runtime context.
    // `connect_async_tls_with_config` constructs a `TcpStream` which
    // needs `Handle::current()` to register against a tokio reactor;
    // gpui's test executor doesn't provide one. We hand the bytes-
    // pushing work off to a tokio task and await it from the gpui
    // executor via `Tokio::spawn_result`. ----
    let client_task = gpui_tokio::Tokio::spawn_result(cx, async move {
        let tls_config = build_client_tls_config(fingerprint);
        let tls_connector = tokio_tungstenite::Connector::Rustls(tls_config);
        let url = format!("wss://127.0.0.1:{}/", bound_addr.port());
        let request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                url.as_str(),
            )?;
        let (mut ws, _resp) = tokio_tungstenite::connect_async_tls_with_config(
            request,
            None,
            false,
            Some(tls_connector),
        )
        .await?;

        let challenge_text = match ws.next().await.expect("challenge frame")? {
            Message::Text(text) => text,
            other => anyhow::bail!("expected text challenge, got {other:?}"),
        };
        let parsed: serde_json::Value = serde_json::from_str(challenge_text.as_ref())?;
        let challenge_hex = parsed["challenge"].as_str().expect("challenge hex");
        let challenge_bytes = hex::decode(challenge_hex)?;
        let mut challenge = [0u8; 16];
        challenge.copy_from_slice(&challenge_bytes);

        let response = compute_response(&secret, &challenge);
        ws.send(Message::Text(
            serde_json::json!({
                "type": "response",
                "response": hex::encode(response),
            })
            .to_string()
            .into(),
        ))
        .await?;

        let _welcome = ws.next().await.expect("welcome")?;

        // remote.editor.capabilities
        ws.send(Message::Text(
            r#"{"jsonrpc":"2.0","id":1,"method":"remote.editor.capabilities"}"#.into(),
        ))
        .await?;
        let caps_reply = match ws.next().await.expect("caps reply")? {
            Message::Text(text) => text,
            other => anyhow::bail!("expected text reply, got {other:?}"),
        };
        let caps: serde_json::Value = serde_json::from_str(caps_reply.as_ref())?;

        // remote.lsp.start → -32601
        ws.send(Message::Text(
            r#"{"jsonrpc":"2.0","id":2,"method":"remote.lsp.start"}"#.into(),
        ))
        .await?;
        let banned_reply = match ws.next().await.expect("banned reply")? {
            Message::Text(text) => text,
            other => anyhow::bail!("expected text reply, got {other:?}"),
        };
        let banned: serde_json::Value = serde_json::from_str(banned_reply.as_ref())?;

        ws.close(None).await?;
        anyhow::Ok((caps, banned))
    });
    let (caps, banned) = client_task.await?;

    // ---- 5. Assert on remote.editor.capabilities — real MCP response. ----
    assert_eq!(caps["id"], 1, "id round-trip");
    let structured = caps
        .pointer("/result/structuredContent")
        .expect("structuredContent present");
    let protocol_version = structured
        .get("protocol_version")
        .and_then(|v| v.as_str())
        .expect("protocol_version");
    assert_eq!(
        protocol_version, "2024-11-05",
        "protocol_version from the real CapabilitiesTool, not the R-2 stub"
    );
    assert!(
        structured.get("editor_mcp_version").is_some(),
        "editor_mcp_version present"
    );
    let event_kinds = structured
        .get("supported_event_kinds")
        .and_then(|v| v.as_array())
        .expect("supported_event_kinds array");
    assert!(
        event_kinds
            .iter()
            .any(|v| v.as_str() == Some("agent_session_message_appended")),
        "agent_session_* kind present in capabilities"
    );

    // ---- 6. remote.lsp.start → -32601. ----
    assert_eq!(banned["id"], 2);
    assert_eq!(
        banned["error"]["code"].as_i64(),
        Some(-32601),
        "banned method must return -32601"
    );
    Ok(())
}
