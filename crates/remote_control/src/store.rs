use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use chrono::Utc;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use rand::TryRngCore as _;
use rand::rngs::OsRng;

use crate::cert;
use crate::dispatch::ProxyDispatcher;
use crate::listener::{self, ListenerConfig, ListenerHandle};
use crate::model::{AuthorizedClient, RemoteControlSettings};
use crate::settings;

/// Number of bytes of OS randomness per client secret. 32 bytes → 256 bits
/// of entropy, sized for use as a session key by the future Remote Control
/// listener. Base64-encoded → 44 chars (32 / 3 * 4, padded).
pub const CLIENT_SECRET_BYTES: usize = 32;

pub struct RemoteControlStore {
    settings: RemoteControlSettings,
    fs: Option<Arc<dyn fs::Fs>>,
    /// Live FS watcher task (dropped → watcher stops).
    _watcher: Option<Task<()>>,
    /// Set of payloads we've written to disk. The watcher fires once
    /// per fs event — including the initial-state load and one per
    /// `atomic_write` — and re-reads the *current* file content each
    /// time, so multiple events may carry the same text. We don't want
    /// any of those round-tripped back through `this.settings = parsed`
    /// (the watcher races against newer in-memory mutations whose own
    /// write hasn't yet flushed). The set retains every text we ever
    /// rendered + wrote; an event whose text is in this set is by
    /// definition our own echo and must be ignored. The set is bounded
    /// at `MAX_ECHO_HISTORY` to keep memory finite under pathological
    /// flapping; older entries are evicted by re-rendering eviction.
    self_write_echoes: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Live listener handle, set while `enabled = true`. Dropping it
    /// triggers the shutdown oneshot and tears down the accept loop +
    /// in-flight per-connection tasks.
    listener: Option<ListenerHandle>,
    /// Live cert (fingerprint exposed via `cert_fingerprint`). Persisted
    /// across set_enabled toggles so the fingerprint stays stable for the
    /// QR code as long as the cert file on disk does.
    cert_fingerprint: Option<[u8; 32]>,
    /// Watch sender feeding the listener's per-connection auth path the
    /// live client list. Present iff the listener is running.
    clients_tx: Option<tokio::sync::watch::Sender<Vec<AuthorizedClient>>>,
    /// In-flight listener bootstrap (Token bootstrap is async — TLS gen
    /// can take ~250ms; we don't block the UI thread). Kept as a `Task`
    /// so the store entity drop semantics cancel a pending bootstrap if
    /// the user toggles OFF before bind completes.
    _bootstrap: Option<Task<()>>,
}

#[derive(Clone, Debug)]
pub enum RemoteControlStoreEvent {
    /// Any persisted-state mutation. The UI re-renders on this event.
    Changed,
}

impl EventEmitter<RemoteControlStoreEvent> for RemoteControlStore {}

struct GlobalRemoteControlStore(Entity<RemoteControlStore>);
impl Global for GlobalRemoteControlStore {}

impl RemoteControlStore {
    fn empty() -> Self {
        Self {
            settings: RemoteControlSettings::default(),
            fs: None,
            _watcher: None,
            self_write_echoes: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            listener: None,
            cert_fingerprint: None,
            clients_tx: None,
            _bootstrap: None,
        }
    }

    /// SHA-256 fingerprint of the live TLS cert (set once the listener has
    /// successfully bound). The R-3 QR generator reads this to embed
    /// `server_fp=<base64>` in the `spk-editor-remote://` URL.
    pub fn cert_fingerprint(&self) -> Option<[u8; 32]> {
        self.cert_fingerprint
    }

    /// Address the listener is bound to (set once the listener has
    /// successfully bound). The integration test reads this to know which
    /// port to dial when `server_port` was 0 (OS-assigned).
    pub fn bound_addr(&self) -> Option<SocketAddr> {
        self.listener.as_ref().map(|handle| handle.bound_addr())
    }

    /// True once `set_enabled(true)` has run and the listener is bound.
    /// Note this is decoupled from `settings.enabled` — `enabled` flips
    /// synchronously, the listener takes a tokio bootstrap roundtrip to
    /// start. UI code that wants "the listener is actually accepting
    /// connections" should check this, not `settings.enabled`.
    pub fn listener_is_running(&self) -> bool {
        self.listener.is_some()
    }

    /// Build the global store and start watching the on-disk JSON file. The
    /// global `dyn Fs` must already be installed (it is — see
    /// `crates/zed/src/main.rs`, which sets the FS global well before
    /// `remote_control::init` runs). In tests, instantiate the store via
    /// `RemoteControlStore::new_with_fs` instead of going through `init_global`.
    pub fn init_global(cx: &mut App) {
        let fs = <dyn fs::Fs>::global(cx);
        let store = cx.new(|cx| Self::new_with_fs(fs, cx));
        cx.set_global(GlobalRemoteControlStore(store));
    }

    /// Build a store backed by `fs`, starting its watcher.
    pub fn new_with_fs(fs: Arc<dyn fs::Fs>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::empty();
        this.fs = Some(fs);
        this.spawn_watch(cx);
        this
    }

    pub fn global(cx: &App) -> Entity<RemoteControlStore> {
        cx.global::<GlobalRemoteControlStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<RemoteControlStore>> {
        cx.try_global::<GlobalRemoteControlStore>()
            .map(|g| g.0.clone())
    }

    pub fn settings(&self) -> &RemoteControlSettings {
        &self.settings
    }

    /// Replace the entire settings struct. Emits `Changed` and persists to
    /// disk (best-effort).
    pub fn update_settings(&mut self, settings: RemoteControlSettings, cx: &mut Context<Self>) {
        if settings == self.settings {
            return;
        }
        let clients_changed = self.settings.clients != settings.clients;
        self.settings = settings;
        if clients_changed {
            self.broadcast_clients_if_running();
        }
        self.notify_changed(cx);
        self.save_to_disk(cx).detach();
    }

    pub fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.enabled == enabled {
            return;
        }
        self.settings.enabled = enabled;
        self.notify_changed(cx);
        self.save_to_disk(cx).detach();
        if enabled {
            self.start_listener_async(cx);
        } else {
            self.stop_listener(cx);
        }
    }

    /// Async bootstrap: load-or-generate the TLS cert, build the watch
    /// channel for the live client list, then call into `listener::start_listener`
    /// on the shared tokio runtime (from `gpui_tokio`). Hops back to the
    /// foreground entity to store the resulting `ListenerHandle`.
    fn start_listener_async(&mut self, cx: &mut Context<Self>) {
        // Guard against double-start. Check BOTH the live handle and
        // the in-flight bootstrap task: a rapid toggle-off-then-on
        // sequence may have a bootstrap already running but no handle
        // yet (the bootstrap_listener future is in progress). Without
        // the `_bootstrap` half of this guard, two parallel bootstrap
        // tasks can race in `cert::load_or_generate` and both end up
        // writing different cert/key pairs to disk.
        if self.listener.is_some() || self._bootstrap.is_some() {
            return;
        }
        let Some(fs) = self.fs.clone() else {
            log::warn!(
                target: "remote_control",
                "no fs registered; can't start listener",
            );
            return;
        };
        if gpui_tokio::Tokio::try_handle(cx).is_none() {
            log::warn!(
                target: "remote_control",
                "gpui_tokio runtime not initialised; can't start listener",
            );
            return;
        }
        let server_address = self.settings.server_address.clone();
        let port = self.settings.server_port;
        let clients = self.settings.clients.clone();

        // Build the watch channel up front; even though the listener
        // hasn't started, holding `clients_tx` on the store lets later
        // `add_client` / `remove_client` calls send fresh snapshots
        // through it without an Option-juggling race.
        let (clients_tx, clients_rx) = tokio::sync::watch::channel(clients);
        self.clients_tx = Some(clients_tx);

        // Spawn the tokio-side bootstrap on the global Tokio runtime;
        // `gpui_tokio::Tokio::spawn_result` returns a gpui Task that
        // properly bridges the wake-from-tokio-thread back to the gpui
        // foreground scheduler. Without this bridge, GPUI's test
        // scheduler panics on cross-thread wake (and the production
        // scheduler may silently mis-schedule).
        let bootstrap = gpui_tokio::Tokio::spawn_result(cx, async move {
            bootstrap_listener(fs, server_address.as_deref(), port, clients_rx).await
        });

        let task = cx.spawn(async move |this, cx| {
            let bootstrap_result = bootstrap.await;
            let _ = this.update(cx, |this, cx| {
                // Clear the in-flight marker BEFORE handling the
                // result — the start-guard at the top of this method
                // checks `_bootstrap.is_some()`, and leaving it set
                // here would block subsequent toggle-on calls.
                this._bootstrap = None;
                match bootstrap_result {
                    Ok((handle, fingerprint)) => {
                        if !this.settings.enabled {
                            // Toggled off while we were starting up; drop the
                            // freshly-built handle immediately.
                            log::info!(
                                target: "remote_control",
                                "listener started but enabled was toggled off; dropping",
                            );
                            return;
                        }
                        log::info!(
                            target: "remote_control",
                            "listener bound on {}",
                            handle.bound_addr(),
                        );
                        this.listener = Some(handle);
                        this.cert_fingerprint = Some(fingerprint);
                        this.notify_changed(cx);
                    }
                    Err(err) => {
                        log::warn!(
                            target: "remote_control",
                            "listener start failed: {err:#}",
                        );
                        this.settings.enabled = false;
                        this.clients_tx = None;
                        this.notify_changed(cx);
                        this.save_to_disk(cx).detach();
                    }
                }
            });
        });
        self._bootstrap = Some(task);
    }

    fn stop_listener(&mut self, cx: &mut Context<Self>) {
        if self.listener.take().is_some() {
            self.cert_fingerprint = None;
            self.clients_tx = None;
            self._bootstrap = None;
            self.notify_changed(cx);
        }
    }

    /// Push the current client list through the watch channel, if the
    /// listener is alive. Called whenever `clients[]` changes so any
    /// future handshake sees the up-to-date set.
    fn broadcast_clients_if_running(&self) {
        let Some(tx) = self.clients_tx.as_ref() else {
            return;
        };
        // `send_replace` doesn't error even if there are no receivers —
        // the listener task always holds one. Ignore the error case
        // defensively (e.g. listener died after a bind failure we logged).
        let _ = tx.send(self.settings.clients.clone());
    }

    pub fn set_address(&mut self, address: Option<String>, cx: &mut Context<Self>) {
        let normalized = address.and_then(|raw| {
            let trimmed = raw.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        });
        if self.settings.server_address == normalized {
            return;
        }
        self.settings.server_address = normalized;
        self.notify_changed(cx);
        self.save_to_disk(cx).detach();
    }

    pub fn set_port(&mut self, port: u16, cx: &mut Context<Self>) {
        if self.settings.server_port == port {
            return;
        }
        self.settings.server_port = port;
        self.notify_changed(cx);
        self.save_to_disk(cx).detach();
    }

    /// Add a new authorized client with the given name. Returns the freshly
    /// constructed client (so the UI can show its secret prefix immediately).
    /// Fails when a client with the same name already exists, or when the
    /// platform's OS RNG isn't available.
    pub fn add_client(&mut self, name: String, cx: &mut Context<Self>) -> Result<AuthorizedClient> {
        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            return Err(anyhow!("client name cannot be empty"));
        }
        if self
            .settings
            .clients
            .iter()
            .any(|existing| existing.name == trimmed)
        {
            return Err(anyhow!("client {trimmed:?} already exists"));
        }
        let secret_base64 = generate_secret()?;
        let client = AuthorizedClient {
            name: trimmed,
            secret_base64,
            created_at: Utc::now(),
        };
        self.settings.clients.push(client.clone());
        self.broadcast_clients_if_running();
        self.notify_changed(cx);
        self.save_to_disk(cx).detach();
        Ok(client)
    }

    pub fn remove_client(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        let before = self.settings.clients.len();
        self.settings.clients.retain(|client| client.name != name);
        let removed = self.settings.clients.len() < before;
        if removed {
            self.broadcast_clients_if_running();
            self.notify_changed(cx);
            self.save_to_disk(cx).detach();
        }
        removed
    }

    fn notify_changed(&self, cx: &mut Context<Self>) {
        cx.emit(RemoteControlStoreEvent::Changed);
        cx.notify();
    }

    fn save_to_disk(&self, cx: &App) -> Task<()> {
        let Some(fs) = self.fs.clone() else {
            log::warn!("remote_control: no fs registered, skipping persistence");
            return Task::ready(());
        };
        let path = paths::remote_control_settings_file().clone();
        let text = settings::render(&self.settings);
        // Stash the to-be-written text BEFORE the async write so the
        // watcher can identify the echo when it fires. The watcher emits
        // one initial-state read + one per atomic-write, each carrying
        // the file's current content — so the same text can be replayed
        // multiple times for a single write. We use a content-set
        // (rather than a FIFO) so re-emissions are all squelched. The
        // set is bounded to defend against rare-but-finite
        // mutation-flood scenarios; eviction is LRU-on-insert.
        const MAX_ECHO_HISTORY: usize = 32;
        if let Ok(mut guard) = self.self_write_echoes.lock() {
            if guard.len() >= MAX_ECHO_HISTORY {
                // Drop an arbitrary entry — HashSet has no order, but
                // worst case we miss-squelch a single stale event, which
                // is harmless (the parsed `==` settings means no apply).
                if let Some(arbitrary) = guard.iter().next().cloned() {
                    guard.remove(&arbitrary);
                }
            }
            guard.insert(text.clone());
        }
        cx.background_spawn(async move {
            if let Some(parent) = path.parent() {
                if let Err(err) = fs.create_dir(parent).await {
                    log::warn!("remote_control: creating {parent:?}: {err:#}");
                    return;
                }
            }
            if let Err(err) = fs.atomic_write(path.clone(), text).await {
                log::warn!("remote_control: writing {path:?}: {err:#}");
            }
        })
    }

    fn spawn_watch(&mut self, cx: &mut Context<Self>) {
        let Some(fs) = self.fs.clone() else {
            return;
        };
        let path = paths::remote_control_settings_file().clone();
        let task = cx.spawn(async move |this, cx| {
            let (mut contents_rx, _watcher) =
                ::settings::watch_config_file(cx.background_executor(), fs, path);
            while let Some(text) = contents_rx.next().await {
                if text.trim().is_empty() {
                    // Spurious initial-empty read (file doesn't exist yet);
                    // nothing to apply.
                    continue;
                }
                let parsed = match settings::parse(&text) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        log::warn!("remote_control: parsing on-disk file: {err:#}");
                        continue;
                    }
                };
                // Squelch the watcher's echo of our own writes. Without
                // this guard the watcher fires after every `save_to_disk`
                // and the apply-from-disk path can run AFTER a newer
                // in-memory mutation has happened (rapid sequences of
                // `set_address` → `set_port` → `add_client` → `set_enabled`
                // all dispatch async writes; the watcher catches up out of
                // order with respect to our in-memory edits). We detect
                // "this is our own write coming back" by checking the
                // parsed value against what the entity currently holds
                // and against the most recently saved snapshot. If the
                // parsed settings equal the last value we wrote, the
                // event is our echo — drop it.
                if this
                    .update(cx, |this, cx| {
                        // An event whose text is in the echo set is by
                        // construction one of our own writes coming back
                        // through the watcher; ignore it (the in-memory
                        // settings are by definition at or ahead of it).
                        let is_self_echo = this
                            .self_write_echoes
                            .lock()
                            .ok()
                            .map(|guard| guard.contains(text.as_str()))
                            .unwrap_or(false);
                        if is_self_echo {
                            return;
                        }
                        if this.settings == parsed {
                            return;
                        }
                        // External edit reached us. Track what changed so
                        // the listener can be started / stopped / pushed
                        // new clients to mirror the in-memory mutator
                        // path (`set_enabled` / `add_client` / …) —
                        // without this the watcher silently desynced
                        // disk state from the running listener (file says
                        // `enabled: true` but no bind happens).
                        let prev_enabled = this.settings.enabled;
                        let prev_clients = this.settings.clients.clone();
                        this.settings = parsed;
                        this.notify_changed(cx);
                        if this.settings.enabled && !prev_enabled {
                            this.start_listener_async(cx);
                        } else if !this.settings.enabled && prev_enabled {
                            this.stop_listener(cx);
                        } else if this.settings.clients != prev_clients {
                            this.broadcast_clients_if_running();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self._watcher = Some(task);
    }
}

async fn bootstrap_listener(
    fs: Arc<dyn fs::Fs>,
    server_address: Option<&str>,
    port: u16,
    clients_rx: tokio::sync::watch::Receiver<Vec<AuthorizedClient>>,
) -> Result<(ListenerHandle, [u8; 32])> {
    let cert = cert::load_or_generate(&fs, server_address).await?;
    let fingerprint = cert.fingerprint_sha256;

    // Per ADR-0003 anti-pattern §"Don't bind to 0.0.0.0 by default": the
    // user toggled ON explicitly, so binding `0.0.0.0` is fine here — the
    // anti-pattern is about not auto-binding when the toggle is off. We
    // bind `0.0.0.0` so the listener is LAN-reachable; the
    // `server_address` field is for QR advertising only.
    let bind_addr: SocketAddr = SocketAddr::from(([0, 0, 0, 0], port));

    let dispatcher: Arc<dyn crate::dispatch::RemoteDispatcher> = ProxyDispatcher::new();

    let cfg = ListenerConfig {
        bind_addr,
        cert,
        clients_rx,
        dispatcher,
    };

    // `start_listener` calls `TcpListener::bind` which needs to be on a
    // tokio runtime — `gpui_tokio::Tokio::spawn_result` (the caller)
    // ensures this future runs on the tokio worker threads, so the
    // implicit runtime is already in scope.
    let handle = listener::start_listener(cfg).await?;
    Ok((handle, fingerprint))
}

fn generate_secret() -> Result<String> {
    let mut buf = [0u8; CLIENT_SECRET_BYTES];
    OsRng
        .try_fill_bytes(&mut buf)
        .map_err(|err| anyhow!("OS RNG unavailable: {err}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;
    use std::collections::HashSet;

    fn new_store(cx: &mut TestAppContext) -> Entity<RemoteControlStore> {
        cx.new(|_| RemoteControlStore::empty())
    }

    #[gpui::test]
    fn secret_length_and_distinctness(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let (a, b) = store.update(cx, |store, cx| {
            let a = store.add_client("Phone".into(), cx).expect("add a");
            let b = store.add_client("Tablet".into(), cx).expect("add b");
            (a, b)
        });

        // Base64 of 32 random bytes is always 44 ASCII chars (no padding ambiguity).
        const EXPECTED_BASE64_LEN: usize = 44;
        assert_eq!(a.secret_base64.len(), EXPECTED_BASE64_LEN);
        assert_eq!(b.secret_base64.len(), EXPECTED_BASE64_LEN);

        // The encoded value must decode back to exactly 32 raw bytes.
        let raw_a = base64::engine::general_purpose::STANDARD
            .decode(&a.secret_base64)
            .expect("decode a");
        assert_eq!(raw_a.len(), CLIENT_SECRET_BYTES);
        let raw_b = base64::engine::general_purpose::STANDARD
            .decode(&b.secret_base64)
            .expect("decode b");
        assert_eq!(raw_b.len(), CLIENT_SECRET_BYTES);

        // Different invocations must not collide. (256 bits of entropy → P
        // (collision) over 2 draws is ~2⁻²⁵⁶; test is effectively deterministic.)
        assert_ne!(a.secret_base64, b.secret_base64);
    }

    #[gpui::test]
    fn add_then_remove_client(cx: &mut TestAppContext) {
        let store = new_store(cx);
        store.update(cx, |store, cx| {
            store.add_client("Phone".into(), cx).expect("added");
            assert_eq!(store.settings().clients.len(), 1);
            assert!(store.remove_client("Phone", cx));
            assert!(store.settings().clients.is_empty());
            // Removing a missing client is a no-op returning false.
            assert!(!store.remove_client("Phone", cx));
        });
    }

    #[gpui::test]
    fn add_client_rejects_duplicate(cx: &mut TestAppContext) {
        let store = new_store(cx);
        store.update(cx, |store, cx| {
            store.add_client("Phone".into(), cx).expect("first");
            let err = store
                .add_client("Phone".into(), cx)
                .expect_err("duplicate must error");
            assert!(format!("{err:#}").contains("already exists"));
        });
    }

    #[gpui::test]
    fn add_client_rejects_empty_name(cx: &mut TestAppContext) {
        let store = new_store(cx);
        store.update(cx, |store, cx| {
            assert!(store.add_client("".into(), cx).is_err());
            assert!(store.add_client("   ".into(), cx).is_err());
        });
    }

    #[gpui::test]
    fn set_enabled_emits_change_event(cx: &mut TestAppContext) {
        use std::sync::{Arc, Mutex};
        let events: Arc<Mutex<Vec<RemoteControlStoreEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let store = new_store(cx);
        let _subscription = store.update(cx, |_, cx| {
            let events = events.clone();
            cx.subscribe(
                &cx.entity(),
                move |_, _, event: &RemoteControlStoreEvent, _| {
                    events.lock().expect("lock").push(event.clone());
                },
            )
        });

        store.update(cx, |store, cx| {
            store.set_enabled(true, cx);
            assert!(store.settings().enabled);
            // Setting the same value again is a no-op (no extra event).
            store.set_enabled(true, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            events.lock().expect("lock").len(),
            1,
            "expected exactly one Changed event"
        );
    }

    #[gpui::test]
    fn set_address_normalizes_empty_to_none(cx: &mut TestAppContext) {
        let store = new_store(cx);
        store.update(cx, |store, cx| {
            store.set_address(Some("203.0.113.1".into()), cx);
            assert_eq!(
                store.settings().server_address.as_deref(),
                Some("203.0.113.1")
            );
            store.set_address(Some("".into()), cx);
            assert!(store.settings().server_address.is_none());
            store.set_address(Some("   \t".into()), cx);
            assert!(store.settings().server_address.is_none());
            store.set_address(Some("198.51.100.5 ".into()), cx);
            assert_eq!(
                store.settings().server_address.as_deref(),
                Some("198.51.100.5"),
                "trailing whitespace must be stripped"
            );
        });
    }

    #[gpui::test]
    fn set_port_updates(cx: &mut TestAppContext) {
        let store = new_store(cx);
        store.update(cx, |store, cx| {
            assert_eq!(store.settings().server_port, crate::model::DEFAULT_PORT);
            store.set_port(9090, cx);
            assert_eq!(store.settings().server_port, 9090);
        });
    }

    #[gpui::test]
    async fn set_enabled_starts_and_stops_listener(cx: &mut TestAppContext) {
        // The listener bootstrap spawns work on tokio's worker threads;
        // wakeups arrive cross-thread, which the deterministic test
        // scheduler refuses by default. `allow_parking` is the supported
        // escape hatch — used widely in tree (`db`, `acp_thread`,
        // `git_graph`, etc.) for the same shape: external runtime work
        // crossing into a test gpui executor.
        cx.executor().allow_parking();
        cx.update(gpui_tokio::init);
        let fs: Arc<dyn fs::Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let store = cx.new(|cx| RemoteControlStore::new_with_fs(fs, cx));

        store.update(cx, |store, cx| {
            store.set_address(Some("127.0.0.1".into()), cx);
            store.set_port(0, cx);
            store.add_client("Test".into(), cx).expect("add");
        });

        store.update(cx, |store, cx| store.set_enabled(true, cx));

        // Bootstrap is async — poll until the listener handle materialises.
        let bound_addr = poll_until(cx, &store, |store| store.bound_addr()).await;
        let bound_addr = bound_addr.expect("listener must bind");
        assert!(bound_addr.port() > 0, "port must be OS-assigned");
        let fingerprint = store
            .read_with(cx, |store, _| store.cert_fingerprint())
            .expect("fingerprint must be set when listener is running");
        assert_eq!(fingerprint.len(), 32);

        // Toggling off drops the listener.
        store.update(cx, |store, cx| store.set_enabled(false, cx));
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert!(!store.listener_is_running());
            assert!(store.cert_fingerprint().is_none());
            assert!(store.bound_addr().is_none());
        });
    }

    async fn poll_until<T, F>(
        cx: &mut TestAppContext,
        store: &Entity<RemoteControlStore>,
        mut predicate: F,
    ) -> Option<T>
    where
        F: FnMut(&RemoteControlStore) -> Option<T>,
    {
        // Generous: the bootstrap path does TLS keypair gen which is
        // single-digit-ms but the executor still has to flush.
        for _ in 0..200 {
            cx.run_until_parked();
            let snapshot = store.read_with(cx, |store, _| predicate(store));
            if snapshot.is_some() {
                return snapshot;
            }
            cx.background_executor
                .timer(std::time::Duration::from_millis(25))
                .await;
        }
        None
    }

    #[gpui::test]
    fn secrets_are_non_colliding_across_many_draws(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let secrets: HashSet<String> = store.update(cx, |store, cx| {
            (0..16)
                .map(|i| {
                    store
                        .add_client(format!("Client{i}"), cx)
                        .expect("add")
                        .secret_base64
                })
                .collect()
        });
        assert_eq!(secrets.len(), 16, "16 fresh secrets should all be unique");
    }
}
