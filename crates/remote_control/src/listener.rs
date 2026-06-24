//! Network listener for Remote Control — TCP accept → TLS 1.3 →
//! WebSocket upgrade → HMAC handshake → JSON-RPC request loop. ADR-0003
//! is the load-bearing reference for every layer.
//!
//! Concurrency shape: one `accept_loop` task drives `TcpListener::accept`
//! in a `tokio::select!` against the shutdown oneshot; per-connection
//! work spawns onto a new tokio task whose lifetime is bounded by the
//! shutdown receiver (cloned via `tokio::sync::broadcast`-equivalent: a
//! `watch` channel where `borrow().clone()` cheaply propagates shutdown
//! intent to every alive connection).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use futures::{SinkExt as _, StreamExt as _};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::allow_list;
use crate::auth;
use crate::cert::ServerCert;
use crate::dispatch::{ConnectionDispatcher, JsonRpcResponse, RemoteDispatcher, parse_request};
use crate::model::AuthorizedClient;

/// Max time (seconds) the client has to reply to the challenge frame
/// before we drop the connection.
const HANDSHAKE_TIMEOUT_SECS: u64 = 10;

/// Idle-read timeout once authenticated. A connection that doesn't send
/// anything for this long is dropped; clients are expected to ping
/// (`remote.editor.ping`) well below this bound to stay alive.
const IDLE_READ_TIMEOUT_SECS: u64 = 60;

/// Exponential backoff for repeat auth failures, in seconds. There is a
/// 1-failure grace period: the FIRST failure from a subnet earns no ban
/// at all (it only increments the counter). The ladder then maps the
/// failure count to a tier as: failure #2 → 30 s, #3 → 5 min, #4 → 1 h,
/// #5 and beyond → 24 h (clamped to the last entry). The schedule
/// discourages persistent scanners (cost grows fast) while keeping
/// accidental fat-finger fail-once cases entirely free.
const BAN_BACKOFF_SECS: &[u64] = &[30, 300, 3_600, 86_400];

/// Window after a ban expires during which the failure-count is still
/// remembered. If a new auth failure arrives within this window, the
/// counter advances and the next ban is the next step in
/// [`BAN_BACKOFF_SECS`]. After the window, the record decays back to
/// "first offense".
const BAN_MEMORY_SECS: u64 = 6 * 3_600;

/// Hard cap on how many subnet records the ban map holds. Without this,
/// a flooder with millions of IPs could OOM the server BY ITS BAN MAP
/// alone. On overflow we evict the record with the oldest `last_seen`
/// (LRU), preserving recent offenders for the backoff escalation.
const BAN_LIST_MAX_ENTRIES: usize = 10_000;

/// Tokens-per-second for the global accept-rate limiter. New accepts
/// that arrive faster than this wait in a tokio sleep before being
/// admitted into TLS — caps total TLS-handshake CPU regardless of how
/// many source IPs are attacking. 5/s easily covers a human pairing
/// + reconnect spike but kills a 10k-IP fan-in cold.
const ACCEPT_RATE_PER_SEC: u32 = 5;

/// Bucket depth for the rate limiter. Allows short bursts above
/// [`ACCEPT_RATE_PER_SEC`] (legit client open + browser tab open at
/// the same time) without queueing.
const ACCEPT_BURST: u32 = 10;

/// Max number of TLS handshakes happening at the same time. A new
/// connection past this bound waits on the semaphore until an earlier
/// handshake either completes or times out. Caps memory + CPU during
/// a flood.
const TLS_HANDSHAKE_CONCURRENCY: usize = 4;

/// Configuration for a listener. Owned by the caller across the
/// `start_listener` call boundary; consumed into the spawned task.
pub struct ListenerConfig {
    pub bind_addr: SocketAddr,
    pub cert: ServerCert,
    /// Receiver of the live authorised-client list. The listener reads
    /// `borrow().clone()` at handshake time, so revoking a client
    /// (`clients_tx.send(new_list)`) takes effect on the NEXT connection.
    /// Open connections from a revoked client are NOT kicked — that's a
    /// future improvement.
    pub clients_rx: watch::Receiver<Vec<AuthorizedClient>>,
    pub dispatcher: Arc<dyn RemoteDispatcher>,
}

/// Per-connection registration. We hold one slot per authenticated
/// client name (1-client-1-connection policy): a fresh authenticated
/// connection from client X always kicks any previous slot whose
/// `client_name` matches X. This eliminates the need for a user-facing
/// "max connections" knob — the budget is implicit (= number of paired
/// clients).
struct ConnectionSlot {
    peer: SocketAddr,
    /// The authenticated client's name. Two slots can never share the
    /// same name simultaneously; the accept loop enforces this on
    /// post-auth slot insertion.
    client_name: String,
    /// Unix-millis timestamp of the last inbound frame on this
    /// connection. Updated by the per-conn task; read by debug logging
    /// + future observability surfaces.
    last_activity: Arc<AtomicI64>,
    /// Drop-on-eviction signal. The connection task selects against
    /// `kill_rx`; firing it sends a clean close frame and exits. We
    /// fire it in three places: (a) a same-client new connection
    /// replacing this one, (b) a revoke that removes this client from
    /// `clients_rx`'s list, (c) future ops affordances.
    kill: Option<oneshot::Sender<()>>,
}

/// One row in the ban map. Keyed by [`subnet_key`] (a /24 for IPv4, /64
/// for IPv6) so a casual attacker can't trivially sidestep by switching
/// their last octet.
struct BanRecord {
    /// How many auth failures we've seen from this subnet within the
    /// memory window. Indexes into [`BAN_BACKOFF_SECS`] for the next
    /// ban duration (clamped to the last entry).
    consecutive_failures: u32,
    /// `Some(t)` while the subnet is currently banned (t is when the
    /// ban lifts). `None` between bans, but the record sticks around
    /// for [`BAN_MEMORY_SECS`] so a repeat offender escalates.
    banned_until: Option<Instant>,
    /// Last time anything touched this record. Used for the LRU
    /// eviction when the map is at [`BAN_LIST_MAX_ENTRIES`] capacity.
    last_seen: Instant,
}

struct ListenerState {
    /// Subnet -> ban record. Entries pruned lazily on every accept (so
    /// the map decays automatically when nobody's attacking) and capped
    /// at [`BAN_LIST_MAX_ENTRIES`] entries via LRU eviction on insert.
    bans: AsyncMutex<HashMap<IpAddr, BanRecord>>,
    /// Live authenticated connections — what the connection-budget LRU
    /// evicter looks at. Populated AFTER successful auth so a failed
    /// handshake never costs a legit client their slot.
    active_conns: AsyncMutex<Vec<ConnectionSlot>>,
    /// Bounds total TLS-handshake concurrency to avoid CPU + memory
    /// thrashing under flood. New connections past the budget queue on
    /// `acquire().await` until an earlier handshake finishes (or
    /// times out at [`HANDSHAKE_TIMEOUT_SECS`]).
    tls_handshake_slots: Semaphore,
    /// Token bucket for the global accept rate. Reads/writes go through
    /// a tokio mutex; the data inside is tiny (last refill time + token
    /// count) so contention is negligible.
    accept_bucket: AsyncMutex<TokenBucket>,
}

impl Default for ListenerState {
    fn default() -> Self {
        Self {
            bans: AsyncMutex::new(HashMap::new()),
            active_conns: AsyncMutex::new(Vec::new()),
            tls_handshake_slots: Semaphore::new(TLS_HANDSHAKE_CONCURRENCY),
            accept_bucket: AsyncMutex::new(TokenBucket::new(ACCEPT_BURST, ACCEPT_RATE_PER_SEC)),
        }
    }
}

/// Trivial in-memory token bucket. `capacity` is the burst budget;
/// `refill_per_sec` is the steady-state rate. `take_one().await` blocks
/// the caller until at least one token is available, refilling lazily
/// from the wall clock since the last call.
struct TokenBucket {
    capacity: u32,
    refill_per_sec: u32,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: u32, refill_per_sec: u32) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity as f64,
            last_refill: Instant::now(),
        }
    }

    /// Refill from elapsed wall time. Returns the wait duration the
    /// caller should sleep for before proceeding, or `Duration::ZERO`
    /// if a token was already available.
    ///
    /// Important: in the wait branch we DECREMENT `tokens` (going
    /// negative) so concurrent callers serialise on the bucket's
    /// shared deficit. Without that, two callers contending on an
    /// empty bucket each independently compute the same wait, sleep
    /// in parallel, and BOTH proceed — silently doubling the
    /// effective rate. With the deficit, the second caller's
    /// `1.0 - self.tokens` is larger, so its wait is correspondingly
    /// longer, restoring the documented rate invariant.
    fn try_take(&mut self) -> Duration {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.last_refill = now;
        let refill = elapsed.as_secs_f64() * self.refill_per_sec as f64;
        self.tokens = (self.tokens + refill).min(self.capacity as f64);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Duration::ZERO
        } else {
            let needed = 1.0 - self.tokens;
            let wait_secs = needed / self.refill_per_sec as f64;
            // Reserve the next token by debiting the bucket NOW.
            // Subsequent callers see the deficit and wait longer.
            self.tokens -= 1.0;
            Duration::from_secs_f64(wait_secs)
        }
    }
}

/// Mask a peer IP down to the network portion we treat as "one
/// offender". /24 for IPv4 (last octet zeroed); /64 for IPv6 (last
/// 8 bytes zeroed). Same /24 over residential ISP usually = same
/// human + same NAT; legit pairs always share their /24 with their
/// own router.
fn subnet_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], 0))
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
    }
}

fn now_millis() -> i64 {
    // `duration_since(UNIX_EPOCH)` fails iff the system clock is set
    // before 1970. In that absurd case the fallback collapses every
    // last_activity to epoch — harmless for LRU comparisons (still
    // monotonic within a stuck-clock session), and the next clock
    // tick after the user fixes their system clock will sort the
    // distortion out on its own. Not worth promoting to Instant
    // (which would require typed plumbing through the slot struct);
    // wall-clock millis are good enough for "show me the staler one".
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    dur.as_millis().min(i64::MAX as u128) as i64
}

/// Handle returned by `start_listener`. Dropping it triggers shutdown.
pub struct ListenerHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    bound_addr: SocketAddr,
    task: Option<JoinHandle<()>>,
}

impl ListenerHandle {
    pub fn bound_addr(&self) -> SocketAddr {
        self.bound_addr
    }
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            // Receiver may already be dropped if the task exited on its own
            // (e.g. accept errored out). Ignoring the send error is correct.
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Bind a TCP listener, build a TLS acceptor, and start the accept loop.
/// The returned handle owns the loop; dropping it shuts down the
/// listener and any in-flight connections.
pub async fn start_listener(cfg: ListenerConfig) -> Result<ListenerHandle> {
    let listener = TcpListener::bind(cfg.bind_addr)
        .await
        .with_context(|| format!("binding {:?}", cfg.bind_addr))?;
    let bound_addr = listener.local_addr().context("reading bound local_addr")?;

    let server_config = build_tls_server_config(&cfg.cert).context("building TLS ServerConfig")?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let state = Arc::new(ListenerState::default());

    // Spawn the kick-on-revoke watcher: when the authorized-client
    // list changes, close any live sessions for clients that no
    // longer appear in it. Without this, a deleted phone would keep
    // its socket alive until the next 60 s idle timeout AND would
    // hammer the ban-list on every retry attempt with the freshly-
    // invalidated secret (catching collateral devices on the same
    // /24 in the cooldown).
    tokio::spawn(watch_revocations(cfg.clients_rx.clone(), state.clone()));

    let task = tokio::spawn(accept_loop(
        listener,
        acceptor,
        cfg.clients_rx,
        cfg.dispatcher,
        state,
        shutdown_rx,
    ));

    Ok(ListenerHandle {
        shutdown_tx: Some(shutdown_tx),
        bound_addr,
        task: Some(task),
    })
}

fn build_tls_server_config(cert: &ServerCert) -> Result<ServerConfig> {
    let cert_der = CertificateDer::from(cert.cert_der.clone());
    let key_der = PrivateKeyDer::try_from(cert.key_der.clone())
        .map_err(|err| anyhow!("invalid private key: {err}"))?;

    let config = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|err| anyhow!("with_single_cert: {err}"))?;
    Ok(config)
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    clients_rx: watch::Receiver<Vec<AuthorizedClient>>,
    dispatcher: Arc<dyn RemoteDispatcher>,
    state: Arc<ListenerState>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                log::info!(target: "remote_control", "listener shutdown requested");
                break;
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, peer)) => {
                        // 1. Subnet ban — cheapest gate. Drop the TCP
                        //    stream before TLS, even before the rate-
                        //    limiter, so banned subnets pay only SYN/ACK.
                        if is_banned(&state, peer.ip()).await {
                            log::debug!(
                                target: "remote_control",
                                "dropping connection from banned subnet (peer={peer})",
                            );
                            drop(stream);
                            continue;
                        }
                        // 2. Global accept-rate limit. Sleep until a
                        //    token is available so a burst of probes
                        //    can't drag the editor's CPU into TLS-
                        //    handshake exhaustion. The sleep races
                        //    against `shutdown_rx` so a shutdown
                        //    request never has to wait up to 1/RATE
                        //    seconds for the bucket to refill.
                        let wait = {
                            let mut bucket = state.accept_bucket.lock().await;
                            bucket.try_take()
                        };
                        if !wait.is_zero() {
                            log::debug!(
                                target: "remote_control",
                                "accept-rate limit: deferring {peer} by {wait:?}",
                            );
                            tokio::select! {
                                biased;
                                _ = &mut shutdown_rx => {
                                    log::info!(target: "remote_control", "listener shutdown interrupted accept-rate sleep");
                                    break;
                                }
                                _ = tokio::time::sleep(wait) => {}
                            }
                        }

                        let acceptor = acceptor.clone();
                        let clients_rx = clients_rx.clone();
                        let dispatcher = dispatcher.clone();
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_conn(
                                stream,
                                peer,
                                acceptor,
                                clients_rx,
                                dispatcher,
                                state,
                            )
                            .await
                            {
                                log::debug!(
                                    target: "remote_control",
                                    "connection from {peer} ended with: {err:#}",
                                );
                            }
                        });
                    }
                    Err(err) => {
                        // EMFILE / temporary errors shouldn't kill the loop.
                        // Sleep briefly to avoid a hot spin if the OS is
                        // refusing accepts entirely.
                        log::warn!(target: "remote_control", "accept error: {err:#}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
}

/// Check if the subnet `ip` belongs to is currently banned. Side
/// effect: prunes records older than [`BAN_MEMORY_SECS`] (with no
/// active ban) so the map decays during quiet periods.
async fn is_banned(state: &ListenerState, ip: IpAddr) -> bool {
    let mut bans = state.bans.lock().await;
    let now = Instant::now();
    let memory_cutoff = now - Duration::from_secs(BAN_MEMORY_SECS);
    // Drop records that have NEITHER an active ban nor a recent
    // last_seen — they're decayed entries, no reason to keep them.
    bans.retain(|_, rec| {
        rec.banned_until.is_some_and(|t| t > now) || rec.last_seen > memory_cutoff
    });
    let key = subnet_key(ip);
    bans.get(&key)
        .and_then(|rec| rec.banned_until)
        .is_some_and(|deadline| deadline > now)
}

/// Record a fresh auth failure from `ip`'s subnet. Advances the
/// failure counter (subject to the [`BAN_MEMORY_SECS`] decay) and
/// sets `banned_until` based on the matching entry in
/// [`BAN_BACKOFF_SECS`]. On insert at capacity, evicts the
/// least-recently-touched record.
async fn record_auth_failure(state: &ListenerState, ip: IpAddr) {
    let mut bans = state.bans.lock().await;
    let now = Instant::now();
    let key = subnet_key(ip);
    let entry = bans.entry(key).or_insert_with(|| BanRecord {
        consecutive_failures: 0,
        banned_until: None,
        last_seen: now,
    });
    // If the prior ban already ended AND it was long enough ago that
    // the record decayed, the .or_insert above gave us a brand-new
    // counter at 0 → step below makes it 1 (= first offense, 30 s).
    entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
    entry.last_seen = now;
    // 1-failure grace period: the first auth-fail in a window only
    // increments the counter and logs at WARN — no ban yet. The ban
    // ladder kicks in on the SECOND consecutive failure. Why:
    // legitimate mobile clients periodically hit one-off malformed
    // handshakes (a half-open WS after Doze, a stale frame from a
    // pre-restart connection, a partial read that races a tungstenite
    // re-key) and the original "ban on failure #1" was tripping users
    // into the 30s → 5min ladder for purely transient causes — a real
    // 2026-05-19 incident where the maintainer's home IP got locked
    // into the 5-minute escalation tier. A scanner spraying random
    // bytes will rack up failures quickly enough to still get banned;
    // a real user with one bad packet eats a single warn.
    if entry.consecutive_failures == 1 {
        log::warn!(
            target: "remote_control",
            "subnet {key} auth failure #1 — no ban yet (grace period); ladder fires on the next consecutive failure",
        );
        return;
    }
    // `consecutive_failures` is now ≥ 2. Index 0 → 30 s, 1 → 5 min, …
    // The offset of 2 here is what the grace skip earns the table.
    let idx = ((entry.consecutive_failures as usize) - 2).min(BAN_BACKOFF_SECS.len() - 1);
    let step = BAN_BACKOFF_SECS[idx];
    entry.banned_until = Some(now + Duration::from_secs(step));
    log::info!(
        target: "remote_control",
        "subnet {key} banned for {step}s (failure #{count})",
        count = entry.consecutive_failures,
    );
    // Capacity bound: evict LRU record if we just blew past the cap.
    // Worst-case overshoot is 1 (we just inserted) so a single pop is
    // enough. The eviction scan is O(BAN_LIST_MAX_ENTRIES); at the
    // current 10k cap + ≤ 5 inserts/sec from the accept-rate limit,
    // that's ≤ 50k comparisons/sec — negligible. Tied `last_seen`
    // values resolve by HashMap iteration order, which is non-
    // deterministic but acceptable for an LRU approximation.
    if bans.len() > BAN_LIST_MAX_ENTRIES {
        if let Some(victim) = bans
            .iter()
            .min_by_key(|(_, rec)| rec.last_seen)
            .map(|(k, _)| *k)
        {
            bans.remove(&victim);
            log::debug!(
                target: "remote_control",
                "ban map at cap, evicted LRU subnet {victim}",
            );
        }
    }
}

/// Successful auth from `ip` lifts any active ban on its subnet but
/// **preserves the failure counter** within the memory window.
///
/// The /24 granularity means the legit user's phone shares a subnet
/// with everything else behind their home NAT (printer, smart-TV,
/// roommate's laptop). If we fully zeroed the counter on every
/// successful auth, an attacker repeatedly probing from the same /24
/// (e.g. a compromised IoT device on the LAN) would get their
/// escalating ban reset every time the legit user's phone reconnects.
/// Lifting just the active ban window while keeping the counter means:
///   - The legit user immediately regains access (good UX).
///   - The next bad-auth attempt from the same subnet picks up where
///     the counter left off — a persistent attacker still ratchets
///     into hour/day-long bans.
async fn record_auth_success(state: &ListenerState, ip: IpAddr) {
    let mut bans = state.bans.lock().await;
    let key = subnet_key(ip);
    if let Some(rec) = bans.get_mut(&key) {
        rec.banned_until = None;
        rec.last_seen = Instant::now();
    }
}

/// Kick any existing slot whose `client_name` matches `name`. Enforces
/// the 1-client-1-connection invariant — the freshly-authenticated
/// connection always wins, the older one gets a clean close.
async fn kick_existing_for_client(state: &ListenerState, name: &str) {
    let mut conns = state.active_conns.lock().await;
    let mut i = 0;
    while i < conns.len() {
        if conns[i].client_name == name {
            let mut victim = conns.swap_remove(i);
            let victim_peer = victim.peer;
            if let Some(kill) = victim.kill.take() {
                let _ = kill.send(());
            }
            log::info!(
                target: "remote_control",
                "kicking previous connection for client {name:?} ({victim_peer}) — replaced by a fresh auth",
            );
        } else {
            i += 1;
        }
    }
}

/// Watch the authorized-clients list and kick any live sessions whose
/// `client_name` is no longer in the list. Spawned once at start_listener;
/// exits when the clients_rx watch sender drops (i.e. the listener is
/// being shut down).
async fn watch_revocations(
    mut clients_rx: watch::Receiver<Vec<AuthorizedClient>>,
    state: Arc<ListenerState>,
) {
    // Drop the initial snapshot — we want to act on CHANGES only,
    // not on the value present when the watcher boots.
    clients_rx.borrow_and_update();
    loop {
        if clients_rx.changed().await.is_err() {
            return;
        }
        let allowed: std::collections::HashSet<String> =
            clients_rx.borrow().iter().map(|c| c.name.clone()).collect();
        // Collect the kill senders + log info under the active_conns
        // lock, then release the lock BEFORE firing the oneshot
        // sends. Send itself is non-blocking, but releasing first
        // minimises the window where a concurrent
        // `kick_existing_for_client` or accept-side slot push would
        // contend with us.
        let to_kill: Vec<(oneshot::Sender<()>, String, SocketAddr)> = {
            let mut conns = state.active_conns.lock().await;
            let mut kills = Vec::new();
            let mut i = 0;
            while i < conns.len() {
                if !allowed.contains(&conns[i].client_name) {
                    let mut victim = conns.swap_remove(i);
                    if let Some(kill) = victim.kill.take() {
                        kills.push((kill, victim.client_name.clone(), victim.peer));
                    }
                } else {
                    i += 1;
                }
            }
            kills
        };
        for (kill, name, peer) in to_kill {
            let _ = kill.send(());
            log::info!(
                target: "remote_control",
                "kicking revoked client {name:?} ({peer})",
            );
        }
    }
}

async fn handle_conn(
    stream: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    clients_rx: watch::Receiver<Vec<AuthorizedClient>>,
    dispatcher: Arc<dyn RemoteDispatcher>,
    state: Arc<ListenerState>,
) -> Result<()> {
    // Disable Nagle: WS frames are small and latency-sensitive. Rare
    // platforms refuse this (already-closed sockets, exotic kernels);
    // log at debug rather than dying — the connection still works,
    // just with slightly worse interactive latency.
    if let Err(err) = stream.set_nodelay(true) {
        log::debug!(
            target: "remote_control",
            "set_nodelay({peer}) failed: {err:#}",
        );
    }

    // Bound concurrent TLS handshakes — a flood gets queued on the
    // semaphore instead of trampling CPU. `tls_permit` is released by
    // drop at function exit OR explicitly after the WS upgrade
    // succeeds (whichever happens first) — the explicit drop on the
    // happy path lets the next queued connection begin TLS work in
    // parallel with our HMAC handshake + request loop. Do NOT rename
    // to `_permit`: the leading-underscore convention signals
    // "unused" and would invite future contributors to delete it
    // wholesale.
    let tls_permit = state
        .tls_handshake_slots
        .acquire()
        .await
        .map_err(|err| anyhow!("tls semaphore closed: {err}"))?;

    // Bound TLS + WS handshake wall-time. Without these timeouts, a
    // slow-loris peer can trickle TLS bytes forever and hold its
    // permit (and its TCP/TLS state) indefinitely; with the semaphore
    // capped at 4 slots, 4 such peers would lock out all legit
    // pairings.
    //
    // NOTE: We do NOT count pre-handshake transport errors (TLS / WS
    // upgrade failures or timeouts) as auth failures. Mobile clients
    // on flaky LTE lose TCP mid-TLS regularly, and a foreground-driven
    // reconnect storm can produce a burst of these. Counting them as
    // attacks pushed legitimate users into the 30s → 5min → 1h → 24h
    // ban ladder for purely network-induced disconnects. The TLS
    // concurrency semaphore + accept-rate limit already gate abuse
    // here; the ban ladder only fires on `record_auth_failure` calls
    // below, which require the peer to send an actual handshake
    // response.
    let tls_stream = match tokio::time::timeout(
        Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        acceptor.accept(stream),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            return Err(err).context("TLS handshake");
        }
        Err(_) => {
            return Err(anyhow!(
                "TLS handshake timeout after {HANDSHAKE_TIMEOUT_SECS}s"
            ));
        }
    };

    // Cap the WS message size. tokio-tungstenite locks WebSocketConfig
    // at handshake — there's no way to swap to a looser config after
    // auth completes, so the same cap covers BOTH the pre-auth
    // challenge round-trip AND every authenticated message thereafter.
    //
    // Pre-auth concern: tungstenite's default (64 MiB) is a free memory
    // amplifier for an unauth'd peer. Per-IP accept-rate-limit + auth-
    // fail ban + HANDSHAKE_TIMEOUT_SECS already gate the abuse window,
    // so we don't need a tight pre-auth-only cap to be safe.
    //
    // Post-auth concern: chunked uploads ship payload bytes as raw WS
    // binary frames (16-byte header + body) per
    // `docs/plans/2026-05-19-chunked-upload-binary-frames.md`. Chunks are
    // sized at ~1 MiB on the client so a single frame round-trip
    // dominates over framing overhead; the cap is 1 MiB so an authed
    // peer can't amplify per-frame memory beyond what we've sized the
    // server for. Big attachments now arrive as a stream of small frames
    // instead of one giant base64-stuffed JSON message, so the 32 MiB
    // headroom the inline-base64 path needed is gone.
    let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_frame_size(Some(1024 * 1024))
        .max_message_size(Some(1024 * 1024));
    let mut ws = match tokio::time::timeout(
        Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        tokio_tungstenite::accept_async_with_config(tls_stream, Some(ws_config)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        // See the TLS branch above: pre-handshake transport errors are
        // network-side glitches (or, at worst, a scanner that hasn't
        // yet sent anything we can identify as malicious), not auth
        // failures. The HMAC challenge response is the earliest point
        // where a peer commits to a verifiable identity claim — only
        // then can a failure be attributed to "wrong actor".
        Ok(Err(err)) => {
            return Err(err).context("WebSocket upgrade");
        }
        Err(_) => {
            return Err(anyhow!(
                "WebSocket upgrade timeout after {HANDSHAKE_TIMEOUT_SECS}s"
            ));
        }
    };
    // After the WS handshake the TLS handshake is over; release the
    // semaphore early so the next queued connection can begin its
    // own TLS work in parallel with our HMAC handshake + request loop.
    drop(tls_permit);

    // 1. Send challenge.
    let challenge = auth::make_challenge().context("make_challenge")?;
    let challenge_frame = serde_json::json!({
        "type": "challenge",
        "challenge": hex::encode(challenge),
        "v": 1,
    });
    ws.send(Message::Text(challenge_frame.to_string().into()))
        .await
        .context("sending challenge")?;

    // 2. Read response within 10s.
    let response_frame =
        tokio::time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT_SECS), ws.next())
            .await
            .map_err(|_| anyhow!("auth timeout after {HANDSHAKE_TIMEOUT_SECS}s"))?
            .ok_or_else(|| anyhow!("connection closed during handshake"))?
            .context("reading handshake response")?;

    let response_text = match response_frame {
        Message::Text(text) => text,
        other => {
            return Err(anyhow!(
                "expected text frame during handshake, got {other:?}"
            ));
        }
    };

    let parsed = match parse_handshake_response(response_text.as_ref()) {
        Ok(parsed) => parsed,
        Err(err) => {
            // Malformed handshake response counts as an auth failure —
            // ban the subnet (escalating backoff) so a scanner that's
            // spraying random bytes pays compounding cooldowns. The
            // grace period in `record_auth_failure` ensures a single
            // glitch-induced malformed payload only warns; the ladder
            // fires from #2 onward.
            //
            // Log the offending payload (truncated to keep huge garbage
            // from filling the log) and the parse error so the next
            // incident is diagnosable without an strace. Truncation is
            // by chars, not bytes — non-ASCII payloads stay UTF-8 valid.
            let preview: String = response_text.chars().take(200).collect();
            let elided = response_text.chars().count() > 200;
            log::warn!(
                target: "remote_control",
                "malformed handshake response from {peer}: {err:#}; payload preview={preview:?}{}",
                if elided { " (truncated)" } else { "" },
            );
            record_auth_failure(&state, peer.ip()).await;
            return Err(err).context("parsing handshake response");
        }
    };

    // 3. Snapshot the client list at handshake time.
    let clients = clients_rx.borrow().clone();
    let identified = auth::identify_client(&challenge, &parsed.response, &clients);
    let Some(client) = identified else {
        log::info!(
            target: "remote_control",
            "auth failed for peer {peer} — banning subnet",
        );
        record_auth_failure(&state, peer.ip()).await;
        let close = CloseFrame {
            code: CloseCode::Policy,
            reason: "unauthorized".into(),
        };
        let _ = ws.send(Message::Close(Some(close))).await;
        return Ok(());
    };
    let client_name = client.name.clone();
    log::info!(
        target: "remote_control",
        "client {client_name:?} from {peer} authenticated",
    );
    // Successful auth resets the subnet's failure counter so a legit
    // user who fat-fingered their first attempt clears their record.
    record_auth_success(&state, peer.ip()).await;

    // 4. Register the slot AFTER auth (not before). A failed handshake
    //    never costs a legitimate client their connection. Then enforce
    //    1-client-1-connection: kick any existing slot with the same
    //    client_name BEFORE pushing the new one, so a reconnect from
    //    the same phone cleanly replaces the previous (likely stale)
    //    socket.
    kick_existing_for_client(&state, &client_name).await;
    let last_activity = Arc::new(AtomicI64::new(now_millis()));
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    {
        let mut conns = state.active_conns.lock().await;
        conns.push(ConnectionSlot {
            peer,
            client_name: client_name.clone(),
            last_activity: last_activity.clone(),
            kill: Some(kill_tx),
        });
    }
    // Ensure the slot is removed when this function returns, no matter
    // which exit path. Identity is by Arc::ptr_eq on `last_activity`.
    //
    // Drop fires `tokio::spawn`, which panics if the runtime has
    // already shut down — possible if the editor process is exiting
    // and the runtime is torn down before the per-connection task.
    // `Handle::try_current()` returns None in that case, in which
    // case we fall back to a blocking_lock-style cleanup attempt;
    // if that also fails we just log and let the slot leak (process
    // is exiting anyway, the leak is bounded by the runtime's
    // lifetime).
    struct SlotGuard {
        state: Arc<ListenerState>,
        last_activity: Arc<AtomicI64>,
    }
    impl Drop for SlotGuard {
        fn drop(&mut self) {
            let state = self.state.clone();
            let last_activity = self.last_activity.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        let mut conns = state.active_conns.lock().await;
                        conns.retain(|c| !Arc::ptr_eq(&c.last_activity, &last_activity));
                    });
                }
                Err(_) => {
                    // No tokio runtime → we're being torn down. Try a
                    // best-effort sync cleanup; if the mutex is held
                    // we can't block forever during shutdown, so we
                    // just log and let the slot orphan with the
                    // dying runtime.
                    if let Ok(mut conns) = state.active_conns.try_lock() {
                        conns.retain(|c| !Arc::ptr_eq(&c.last_activity, &last_activity));
                    } else {
                        log::debug!(
                            target: "remote_control",
                            "SlotGuard::drop during shutdown — runtime gone and active_conns locked; slot orphaned",
                        );
                    }
                }
            }
        }
    }
    let _slot_guard = SlotGuard {
        state: state.clone(),
        last_activity: last_activity.clone(),
    };

    // 5. Welcome. Echo the negotiated compression so the client knows whether
    //    to expect (and produce) compressed binary frames.
    let welcome = match parsed.compress_dict {
        Some(dict) => serde_json::json!({
            "type": "welcome",
            "client": client_name,
            "compress": crate::wire_codec::CODEC_DEFLATE,
            "dict": dict,
        }),
        None => serde_json::json!({ "type": "welcome", "client": client_name }),
    };
    ws.send(Message::Text(welcome.to_string().into()))
        .await
        .context("sending welcome")?;

    // 6. Request loop.
    run_request_loop(
        &mut ws,
        &client_name,
        dispatcher.as_ref(),
        last_activity,
        kill_rx,
        parsed.compress_dict,
    )
    .await?;
    Ok(())
}

/// Highest preset-dictionary id the server implements. Negotiation picks
/// `min(client_dict, SERVER_MAX_DICT)` so a newer client downgrades cleanly.
const SERVER_MAX_DICT: u8 = crate::wire_dict::WIRE_DICT_PROTO_V1;

/// Parsed handshake response: the 32-byte HMAC plus the negotiated outbound
/// compression dictionary id (`Some(dict)` when the client advertised a codec
/// we support, `None` otherwise — older clients omit the field entirely).
struct ParsedHandshake {
    response: [u8; 32],
    compress_dict: Option<u8>,
}

fn parse_handshake_response(text: &str) -> Result<ParsedHandshake> {
    #[derive(serde::Deserialize)]
    struct ResponseFrame {
        #[serde(rename = "type")]
        kind: String,
        response: String,
        /// Wire-compression codecs the client understands. Absent on older
        /// clients → compression stays off.
        #[serde(default)]
        compress: Vec<String>,
        /// Highest preset-dictionary id the client implements.
        #[serde(default)]
        dict: u8,
    }
    let frame: ResponseFrame =
        serde_json::from_str(text).context("decoding response frame as JSON")?;
    if frame.kind != "response" {
        return Err(anyhow!("expected type=\"response\", got {:?}", frame.kind));
    }
    let raw =
        hex::decode(frame.response.trim()).map_err(|err| anyhow!("response hex decode: {err}"))?;
    if raw.len() != 32 {
        return Err(anyhow!(
            "response must be 32 bytes (64 hex chars), got {} bytes",
            raw.len()
        ));
    }
    let mut response = [0u8; 32];
    response.copy_from_slice(&raw);

    // Negotiate compression: only if the client offered our codec. The chosen
    // dictionary is the lower of what each side supports.
    let compress_dict = frame
        .compress
        .iter()
        .any(|c| c == crate::wire_codec::CODEC_DEFLATE)
        .then(|| frame.dict.min(SERVER_MAX_DICT));

    Ok(ParsedHandshake {
        response,
        compress_dict,
    })
}

async fn run_request_loop<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    client_name: &str,
    dispatcher: &dyn RemoteDispatcher,
    last_activity: Arc<AtomicI64>,
    mut kill_rx: oneshot::Receiver<()>,
    compress_dict: Option<u8>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Per-connection dispatcher state (lazy: opened on first request). On
    // open, immediately `take_notifications()` so the select! arm sees
    // the receiver. If opening fails (e.g. local MCP socket missing) we
    // surface -32603 per-request and keep the WS alive — the client may
    // retry, and a flapping editor restart shouldn't kick paired phones.
    let mut conn: Option<Box<dyn ConnectionDispatcher>> = None;
    let mut notifications_rx: Option<tokio::sync::mpsc::Receiver<serde_json::Value>> = None;

    loop {
        // Tokio `select!` here arbitrates between WS-read, notification
        // pump, idle timeout, and the eviction-kill signal. The kill
        // arm fires when the accept loop picked this slot as LRU
        // victim to make room for a new connection.
        let select_outcome = if let Some(rx) = notifications_rx.as_mut() {
            tokio::select! {
                biased;
                _ = &mut kill_rx => SelectOutcome::Evicted,
                next = ws.next() => SelectOutcome::Frame(next),
                notification = rx.recv() => SelectOutcome::Notification(notification),
                _ = tokio::time::sleep(Duration::from_secs(IDLE_READ_TIMEOUT_SECS)) => {
                    SelectOutcome::Idle
                }
            }
        } else {
            tokio::select! {
                biased;
                _ = &mut kill_rx => SelectOutcome::Evicted,
                next = ws.next() => SelectOutcome::Frame(next),
                _ = tokio::time::sleep(Duration::from_secs(IDLE_READ_TIMEOUT_SECS)) => {
                    SelectOutcome::Idle
                }
            }
        };

        match select_outcome {
            SelectOutcome::Frame(None) => {
                log::debug!(
                    target: "remote_control",
                    "client {client_name:?} closed connection",
                );
                return Ok(());
            }
            SelectOutcome::Frame(Some(Err(err))) => {
                return Err(anyhow!("ws read error: {err}"));
            }
            SelectOutcome::Frame(Some(Ok(frame))) => {
                // Any inbound frame counts as activity for LRU
                // eviction purposes — a client mid-conversation
                // shouldn't lose its slot to a fresh connection.
                last_activity.store(now_millis(), Ordering::Relaxed);
                // A JSON-RPC request arrives either as a TEXT frame or, when
                // compression was negotiated, as a compressed BINARY frame.
                // Extract its text here; non-request frames (ping/upload/close)
                // are handled inline and leave `request_text` as None.
                let request_text: Option<String> = match frame {
                    Message::Text(text) => Some(text.to_string()),
                    Message::Ping(payload) => {
                        ws.send(Message::Pong(payload))
                            .await
                            .context("sending pong")?;
                        None
                    }
                    Message::Pong(_) => None,
                    Message::Close(frame) => {
                        log::debug!(
                            target: "remote_control",
                            "client {client_name:?} sent close frame: {frame:?}",
                        );
                        let _ = ws.send(Message::Close(None)).await;
                        return Ok(());
                    }
                    Message::Frame(_) => None,
                    Message::Binary(bytes) => {
                        // Compressed JSON-RPC request (negotiated)? Decode and
                        // route it through the same dispatch path as text.
                        if crate::wire_codec::is_compressed(&bytes) {
                            match crate::wire_codec::decompress(&bytes) {
                                Ok(raw) => match String::from_utf8(raw) {
                                    Ok(text) => Some(text),
                                    Err(err) => {
                                        log::warn!(
                                            target: "remote_control",
                                            "client {client_name:?} sent a compressed frame that wasn't UTF-8: {err}; dropping",
                                        );
                                        None
                                    }
                                },
                                Err(err) => {
                                    log::warn!(
                                        target: "remote_control",
                                        "client {client_name:?} sent an undecodable compressed frame: {err}; dropping",
                                    );
                                    None
                                }
                            }
                        } else {
                        // Chunked-upload frame: 16-byte header
                        // (u64 upload_id BE | u64 offset BE) + raw payload.
                        // See `docs/plans/2026-05-19-chunked-upload-binary-frames.md`.
                        // Anything < 16 bytes is malformed — log and
                        // drop rather than erroring on the WS, since a
                        // legit client never sends shorter frames and
                        // an attacker shouldn't get useful feedback.
                        if bytes.len() < 16 {
                            log::warn!(
                                target: "remote_control",
                                "client {client_name:?} sent {n}-byte binary frame (< 16); dropping",
                                n = bytes.len(),
                            );
                            continue;
                        }
                        // Diagnostic — debug-level breadcrumb so a live
                        // log tail can confirm chunks ARE reaching the
                        // server. Per-chunk, so it stays at debug to
                        // avoid flooding info on large uploads. The
                        // header parse below is duplicated by the
                        // handler; that's fine, this is a debug aid not
                        // a hot path.
                        let upload_id_log =
                            u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8]));
                        let offset_log =
                            u64::from_be_bytes(bytes[8..16].try_into().unwrap_or([0; 8]));
                        log::debug!(
                            target: "remote_control::upload",
                            "binary frame from {client_name:?}: upload_id={upload_id_log} offset={offset_log} payload_bytes={}",
                            bytes.len() - 16,
                        );
                        // Delegate parsing + dispatch to the upper-layer
                        // handler registered via
                        // `remote_control::set_binary_frame_handler`.
                        // Keeps `remote_control` free of the
                        // `solution_agent` dep (which would pull a
                        // second rustls CryptoProvider via its
                        // transitive `agent_servers` / `claude-acp`
                        // graph and break the post-auth handshake).
                        match crate::binary_frame_handler() {
                            Some(handler) => {
                                if let Err(err) = handler(&bytes) {
                                    log::warn!(
                                        target: "remote_control::upload",
                                        "binary frame handler rejected upload_id={upload_id_log} offset={offset_log} (client={client_name:?}): {err}",
                                    );
                                } else {
                                    log::debug!(
                                        target: "remote_control::upload",
                                        "binary frame written: upload_id={upload_id_log} offset={offset_log}",
                                    );
                                }
                            }
                            None => {
                                log::warn!(
                                    target: "remote_control::upload",
                                    "NO binary frame handler installed; dropping {n}-byte frame (client={client_name:?})",
                                    n = bytes.len(),
                                );
                            }
                        }
                            None
                        }
                    }
                };

                // A JSON-RPC request extracted from a text or compressed-binary
                // frame — dispatch it and reply (compressing the reply when
                // negotiated).
                if let Some(text) = request_text {
                    let response = match parse_request(&text) {
                        Ok(req) => {
                            // Lazily open the proxy. On first call we
                            // also grab the notifications receiver.
                            if conn.is_none() {
                                match dispatcher.open_connection().await {
                                    Ok(mut c) => {
                                        notifications_rx = c.take_notifications();
                                        conn = Some(c);
                                    }
                                    Err(err) => {
                                        let response = JsonRpcResponse::error(
                                            req.id.clone(),
                                            -32603,
                                            format!("opening local MCP proxy: {err}"),
                                        );
                                        write_response(ws, &response, compress_dict).await?;
                                        continue;
                                    }
                                }
                            }
                            // Safe: `conn` is Some here.
                            let dispatcher_ref = conn
                                .as_mut()
                                .ok_or_else(|| anyhow!("connection dispatcher disappeared"))?;
                            dispatcher_ref.dispatch(client_name, req).await
                        }
                        Err(parse_err_response) => *parse_err_response,
                    };
                    write_response(ws, &response, compress_dict).await?;
                }
            }
            SelectOutcome::Notification(None) => {
                // Notifications channel closed (proxy reader dropped).
                // Stop pumping; keep the WS alive so the client can
                // still issue RPC calls (each call opens its own
                // upstream frame; the dispatcher will re-fail cleanly).
                notifications_rx = None;
            }
            SelectOutcome::Notification(Some(payload)) => {
                let kind = payload
                    .pointer("/params/kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if kind.starts_with("upload_") {
                    log::info!(
                        target: "remote_control::upload",
                        "forwarding {kind} notification to {client_name:?}",
                    );
                }
                if !allow_list::should_forward_event(kind) {
                    if kind.starts_with("upload_") {
                        log::warn!(
                            target: "remote_control::upload",
                            "DROPPING {kind} notification — allow_list rejected (BUG?)",
                        );
                    }
                    continue;
                }
                let envelope = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "remote/notification",
                    "params": payload
                        .get("params")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                });
                let serialized = match serde_json::to_string(&envelope) {
                    Ok(text) => text,
                    Err(err) => {
                        log::warn!(
                            target: "remote_control",
                            "serialising notification: {err:#}; dropping",
                        );
                        continue;
                    }
                };
                if let Err(err) = send_text_frame(ws, serialized, compress_dict).await {
                    return Err(anyhow!(
                        "sending notification to client {client_name:?}: {err}"
                    ));
                }
            }
            SelectOutcome::Idle => {
                log::info!(
                    target: "remote_control",
                    "client {client_name:?} idle for {IDLE_READ_TIMEOUT_SECS}s, closing",
                );
                let close = CloseFrame {
                    code: CloseCode::Away,
                    reason: "idle timeout".into(),
                };
                let _ = ws.send(Message::Close(Some(close))).await;
                return Ok(());
            }
            SelectOutcome::Evicted => {
                log::info!(
                    target: "remote_control",
                    "client {client_name:?} evicted by accept loop to free a slot",
                );
                let close = CloseFrame {
                    code: CloseCode::Away,
                    reason: "evicted by new connection".into(),
                };
                let _ = ws.send(Message::Close(Some(close))).await;
                return Ok(());
            }
        }
    }
}

enum SelectOutcome {
    Frame(
        Option<
            Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>,
        >,
    ),
    Notification(Option<serde_json::Value>),
    Idle,
    Evicted,
}

async fn write_response<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    response: &JsonRpcResponse,
    compress_dict: Option<u8>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let payload =
        serde_json::to_string(response).map_err(|err| anyhow!("serialising response: {err}"))?;
    send_text_frame(ws, payload, compress_dict)
        .await
        .context("sending response")
}

/// Send one JSON text frame, compressing it to a binary frame when the client
/// negotiated compression (`compress_dict`) and the payload is large enough to
/// benefit. A frame is never inflated, and a client that didn't negotiate
/// compression only ever receives text.
async fn send_text_frame<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    text: String,
    compress_dict: Option<u8>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if let Some(dict) = compress_dict {
        if let Some(frame) = crate::wire_codec::compress_if_worthwhile(
            text.as_bytes(),
            dict,
            crate::wire_codec::DEFAULT_COMPRESS_THRESHOLD_BYTES,
        ) {
            ws.send(Message::Binary(frame.into()))
                .await
                .context("sending compressed frame")?;
            return Ok(());
        }
    }
    ws.send(Message::Text(text.into()))
        .await
        .context("sending text frame")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const HMAC_HEX: &str =
        "abababababababababababababababababababababababababababababababab";

    #[test]
    fn handshake_negotiates_compression_when_client_advertises_deflate() {
        let frame = format!(
            r#"{{"type":"response","response":"{HMAC_HEX}","compress":["deflate"],"dict":1}}"#
        );
        let parsed = parse_handshake_response(&frame).expect("parses");
        assert_eq!(parsed.compress_dict, Some(1));
    }

    #[test]
    fn handshake_leaves_compression_off_for_legacy_client() {
        // No `compress`/`dict` fields — an older client.
        let frame = format!(r#"{{"type":"response","response":"{HMAC_HEX}"}}"#);
        let parsed = parse_handshake_response(&frame).expect("parses");
        assert_eq!(parsed.compress_dict, None);
    }

    #[test]
    fn handshake_downgrades_dict_to_server_max() {
        let frame = format!(
            r#"{{"type":"response","response":"{HMAC_HEX}","compress":["deflate"],"dict":9}}"#
        );
        let parsed = parse_handshake_response(&frame).expect("parses");
        assert_eq!(parsed.compress_dict, Some(SERVER_MAX_DICT));
    }

    #[test]
    fn handshake_ignores_unknown_codec() {
        let frame = format!(
            r#"{{"type":"response","response":"{HMAC_HEX}","compress":["zstd"],"dict":1}}"#
        );
        let parsed = parse_handshake_response(&frame).expect("parses");
        assert_eq!(parsed.compress_dict, None);
    }

    /// Returns the remaining ban duration (in whole seconds) for `ip`'s
    /// subnet, or `None` if the record carries no active ban. Reads the
    /// ban map directly so the assertions don't depend on wall-clock
    /// elapsed time within the test (the deadline is always
    /// `now + tier`, set at the moment `record_auth_failure` ran).
    async fn ban_tier_secs(state: &ListenerState, ip: IpAddr) -> Option<u64> {
        let bans = state.bans.lock().await;
        let rec = bans.get(&subnet_key(ip))?;
        let until = rec.banned_until?;
        // Round up: the deadline is `set_instant + tier`, and a few
        // microseconds of test execution have elapsed since, so the raw
        // remaining duration is just under the tier. Adding the saturating
        // sub-second remainder back recovers the exact tier value.
        let remaining = until.saturating_duration_since(Instant::now());
        Some(remaining.as_secs() + 1)
    }

    fn ip(last_octet: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, last_octet))
    }

    #[tokio::test]
    async fn auth_failure_ban_ladder_climbs_tiers() {
        let state = ListenerState::default();
        let peer = ip(7);

        // Failure #1: grace period — no ban.
        record_auth_failure(&state, peer).await;
        assert_eq!(
            ban_tier_secs(&state, peer).await,
            None,
            "first failure must be a free grace; if this asserts a ban, the grace period regressed"
        );

        // Failure #2: first backoff tier = 30 s. This is the assertion
        // that pins the `- 2` index math: changing it to `- 1` would
        // index BAN_BACKOFF_SECS[0+... ] one step ahead and yield 300,
        // not 30, breaking this check.
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, Some(30));

        // Failure #3: 5 min.
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, Some(300));

        // Failure #4: 1 h.
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, Some(3_600));

        // Failure #5: 24 h (last tier).
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, Some(86_400));

        // Failures #6, #7: stay clamped at the last tier — the index
        // saturates instead of running past the array end (which would
        // panic).
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, Some(86_400));
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, Some(86_400));
    }

    #[tokio::test]
    async fn ban_ladder_matches_backoff_array_with_grace_offset() {
        // Independent of the hard-coded tier values above: drive the
        // ladder and assert each ban equals BAN_BACKOFF_SECS[count - 2],
        // which is the exact mapping the grace offset is supposed to
        // produce. This catches an off-by-one in either direction.
        let state = ListenerState::default();
        let peer = ip(9);

        record_auth_failure(&state, peer).await; // #1, grace
        assert_eq!(ban_tier_secs(&state, peer).await, None);

        for count in 2..=(BAN_BACKOFF_SECS.len() + 2) {
            record_auth_failure(&state, peer).await;
            let expected_idx = (count - 2).min(BAN_BACKOFF_SECS.len() - 1);
            assert_eq!(
                ban_tier_secs(&state, peer).await,
                Some(BAN_BACKOFF_SECS[expected_idx]),
                "failure #{count} should map to BAN_BACKOFF_SECS[{expected_idx}]"
            );
        }
    }

    #[tokio::test]
    async fn distinct_subnets_have_independent_counters() {
        let state = ListenerState::default();
        let a = ip(1);
        // Different /24 (so a distinct subnet_key).
        let b = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1));

        // Two failures on `a` → tier 30 s. `b` is untouched.
        record_auth_failure(&state, a).await;
        record_auth_failure(&state, a).await;
        assert_eq!(ban_tier_secs(&state, a).await, Some(30));
        assert_eq!(ban_tier_secs(&state, b).await, None);

        // First failure on `b` → still in its own grace window.
        record_auth_failure(&state, b).await;
        assert_eq!(ban_tier_secs(&state, b).await, None);
        // `a` is unaffected.
        assert_eq!(ban_tier_secs(&state, a).await, Some(30));
    }

    #[tokio::test]
    async fn failure_count_decays_after_memory_window() {
        // The decay is time-driven via `is_banned`'s `retain`, which
        // prunes records whose ban has lifted AND whose `last_seen` is
        // older than BAN_MEMORY_SECS. Since the record stores a
        // `std::time::Instant` (not a tokio clock we can pause), we
        // simulate elapsed time by back-dating the record's fields
        // directly, then verify the prune fires and the counter resets.
        let state = ListenerState::default();
        let peer = ip(42);

        // Get the counter to #2 (a real ban tier).
        record_auth_failure(&state, peer).await;
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, Some(30));

        // Simulate: the ban has long since lifted and the memory window
        // has fully elapsed. Back-date `last_seen` past the cutoff and
        // clear `banned_until` so the record looks decayed.
        {
            let mut bans = state.bans.lock().await;
            let rec = bans
                .get_mut(&subnet_key(peer))
                .expect("record present after failures");
            rec.banned_until = None;
            rec.last_seen = Instant::now() - Duration::from_secs(BAN_MEMORY_SECS + 60);
        }

        // `is_banned` prunes decayed records as a side effect; the
        // subnet should no longer be banned and the record should be gone.
        assert!(!is_banned(&state, peer).await);
        {
            let bans = state.bans.lock().await;
            assert!(
                !bans.contains_key(&subnet_key(peer)),
                "decayed record should have been pruned by is_banned"
            );
        }

        // A fresh failure now starts the ladder over from the grace
        // period (#1 → no ban), proving the counter reset.
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, None);
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, Some(30));
    }

    #[tokio::test]
    async fn successful_auth_lifts_ban_but_keeps_counter() {
        let state = ListenerState::default();
        let peer = ip(13);

        // Climb to #3 (5 min tier).
        record_auth_failure(&state, peer).await;
        record_auth_failure(&state, peer).await;
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, Some(300));

        // A successful auth lifts the active ban window...
        record_auth_success(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, None);

        // ...but preserves the counter, so the NEXT failure resumes the
        // ladder at #4 (1 h) rather than restarting at the grace tier.
        record_auth_failure(&state, peer).await;
        assert_eq!(ban_tier_secs(&state, peer).await, Some(3_600));
    }
}
