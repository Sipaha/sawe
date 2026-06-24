//! Remote Control: state model + JSON-file persistence + network listener
//! for the editor's external Remote Control surface.
//!
//! R-1 shipped settings + UI; R-1.5 added the QR popover; R-2 (this slice)
//! adds the network listener — TLS 1.3 + WebSocket upgrade + per-client
//! HMAC challenge auth + JSON-RPC dispatch — driven by toggling
//! `RemoteControlStore::set_enabled(true)`. Transport rationale is
//! ADR-0003.

pub mod allow_list;
pub mod auth;
pub mod cert;
pub mod dispatch;
pub mod listener;
mod model;
pub mod proxy;
mod settings;
mod store;
mod wire_codec;
mod wire_dict;

pub use model::{AuthorizedClient, RemoteControlSettings};
pub use store::{RemoteControlStore, RemoteControlStoreEvent};

use gpui::App;
use std::sync::{Arc, OnceLock};

/// Callback invoked on every authenticated WebSocket binary frame the
/// listener receives. Set once at process init by an upper-layer crate
/// that knows how to interpret the bytes (today: `solution_agent`,
/// which parses the 16-byte upload header + dispatches to its
/// `UploadManager`). Returning `Err(reason)` makes the listener log
/// + drop the frame; `Ok(())` is the happy path (the handler is
/// responsible for any further fan-out, e.g. an `upload_chunk_acked`
/// notification).
///
/// The indirection keeps the dep direction clean — `remote_control`
/// is auth + transport only and doesn't know the upload protocol.
/// Inverting via this handler also avoids pulling `solution_agent`
/// into `remote_control`, which would feature-unify a second rustls
/// `CryptoProvider` into the dep set via the transitive
/// `agent_servers` / `claude-acp` graph and break the post-auth
/// handshake on first use.
pub type BinaryFrameHandler = Arc<dyn Fn(&[u8]) -> Result<(), String> + Send + Sync + 'static>;

static BINARY_FRAME_HANDLER: OnceLock<BinaryFrameHandler> = OnceLock::new();

/// Register the process-wide binary-frame handler. Subsequent calls
/// are no-ops — first registration wins (matches `OnceLock` semantics).
/// Call from the upper-layer crate's init AFTER `remote_control::init`
/// so the listener sees the handler before accepting any clients.
pub fn set_binary_frame_handler(handler: BinaryFrameHandler) {
    let _ = BINARY_FRAME_HANDLER.set(handler);
}

pub fn binary_frame_handler() -> Option<BinaryFrameHandler> {
    BINARY_FRAME_HANDLER.get().cloned()
}

pub fn init(cx: &mut App) {
    RemoteControlStore::init_global(cx);
}
