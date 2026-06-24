# R-6c-multi: Multi-server pairing storage + Servers screen

**Status:** ready to dispatch
**Repo:** `sawe-mobile` (sibling)
**Depends on:** R-6b (`EncryptedSharedPreferences` pairing) + R-6a (`ConnectionState` per-client) + R-6d (`AppMasterKey` singleton).
**Goal:** Support multiple paired workstations from one phone app. PairingRepository moves from one-URL to a keyed map. New Servers screen as the entry point when ≥2 servers exist; single-server case keeps the R-6b cold-start auto-resume behavior unchanged.

FCM push notifications are explicitly out of this phase — that's the OTHER half of the original R-6c. Filed as `R-6c-push` for later when the user wants notifications.

## Why this phase exists

Original 2026-05-15 user ask: "Также нужно создать проект `sawe-mobile`... также нужны множественные пары". The single-server R-6b shipping took priority; multi-server is the follow-up.

Realistic scenario: user has a desktop at home + a laptop on the go + a server at the office. One Android phone connects to whichever is online. Currently the phone can only pair with ONE server at a time — re-pairing erases the previous.

## Scope

### A. `PairingRepository` — keyed multi-server storage

Current (R-6b):
```kotlin
class PairingRepository(context, masterKey) {
    fun save(url: String)
    fun load(): String?
    fun clear()
}
```

New shape:
```kotlin
@Serializable
data class PairedServer(
    val id: String,                  // UUID v4 generated at first save, stable across re-pair
    val pairingUrl: String,          // the spk-remote://... string
    val label: String,               // friendly name; defaults to `host:port` from URL parse
    val fingerprintHex: String,      // SHA-256 of cert, hex (for Settings display)
    val firstPairedAtMs: Long,
    val lastConnectedAtMs: Long?,
)

class PairingRepository(context, masterKey) {
    fun loadAll(): List<PairedServer>        // ordered by lastConnectedAtMs DESC (then firstPairedAtMs DESC)
    fun upsert(server: PairedServer)         // by id
    fun remove(serverId: String)
    fun setLastConnected(serverId: String, nowMs: Long)
    fun activeServerId(): String?            // last manually-selected via setActive()
    fun setActive(serverId: String?)
}
```

Storage: `paired_servers_v2` blob in `EncryptedSharedPreferences` (the existing R-6b key was `pairing_url`). On boot:
1. Try `paired_servers_v2` first.
2. If absent AND old `pairing_url` key present → migrate: parse the legacy URL, derive `id` (`UUID.randomUUID().toString()`), persist as a single `PairedServer`, delete the old key. One-shot migration.

`activeServerId`: persisted as a separate key so the user can "switch to laptop" and have that survive restart.

### B. New nav route: `servers` (Servers list screen)

Routes after R-6c:

- `pairing` — pair a new server. Same `QrPairingScreen` from R-5b. On success, ADD to `PairingRepository` (don't overwrite). Then navigate to `solutions` for the newly-paired server.
- **NEW** `servers` — list of paired servers. Tappable rows; tap → set as active + navigate to `solutions`. "+" FAB → `pairing` route to add another. Long-press / swipe row → confirmation → remove.
- `solutions`, `solutions/{id}`, `solutions/{id}/sessions/{sid}`, `settings` — unchanged (operate on the currently-active server).

Cold-start landing logic in `MainActivity`:
- `paired_servers_v2` empty → land on `pairing` (unchanged for first launch UX).
- One paired server → land on `solutions` (R-6b auto-resume behavior preserved for the common case).
- Multiple paired servers → land on `servers` so user picks which to connect to. (Optionally: if `activeServerId` is set AND that server's `lastConnectedAtMs` is recent — say <12h ago — auto-resume directly to that server's `solutions`. Sub-agent's call on the heuristic.)

### C. `ServersListScreen.kt`

Material 3 list:

- TopAppBar "Sawe servers" with overflow menu → Settings (the existing R-6b settings, scoped to the active server).
- Each row: large `label`, secondary `host:port`, fingerprint short form (first/last 4 hex), per-row connection state pill (Connected / Reconnecting / Disconnected — only shows for the currently-active server; other rows show "Tap to connect").
- ExtendedFAB "+" → "Pair new server" → navigates to `pairing`.
- Tap a non-active row → set as active + navigate to `solutions`.

Empty state: shouldn't happen post-multi-server (we route to `pairing` instead) but defensive copy: "No servers paired. Tap + to scan a pairing QR."

### D. `MainViewModel` — active-server lifecycle

Currently `MainViewModel` owns one `RemoteClient`. Replace with:

- `activeServerId: StateFlow<String?>`.
- `private var client: RemoteClient?` — rebuilt on every `activeServerId` change.
- Method `switchToServer(serverId: String)`:
  1. `client?.close()` and cancel reconnect.
  2. Look up `PairedServer` by id.
  3. Build a new `RemoteClient` with that server's pairing URL.
  4. Update `_activeServerId` + `pairingRepository.setActive(serverId)`.
  5. Connect.
- `addServer(rawUrl)` → upsert into repo, switchTo on success.
- `removeServer(serverId)` → repo.remove + if it was active → switch to another or clear.

The `lastSeenRepository`, `draftRepository`, `queueStore`, `navStateRepository` are SCOPED PER SERVER: each repository's key gets a `<serverId>:` prefix. So drafts in server A don't leak into server B. Migration on R-6c boot: if a draft key has no server prefix (left over from R-6b), assume it belongs to the migrated server (the only one).

### E. R-6b Settings screen — minor changes

Currently the Settings screen shows the single paired server. After R-6c:
- "Server info" section shows ACTIVE server's info (gets new "Switch server" button → `servers` route if ≥2 paired).
- "Forget paired server" now means "remove THIS server"; multi-paired case keeps the others.
- "Re-pair" now means "scan another QR" → `pairing` route (which now adds, not replaces).
- New "All servers" section at the bottom (when ≥2 paired): tiny list of other servers with switch buttons.

### F. Connection-state banner — per-active-server

R-6a's Compose banner currently displays the single `ConnectionState`. Still works — only the active server is connected at a time. Make sure the banner clears when switching servers (`Connecting...` should show during the switch).

### G. Wire-shape — none

No server-side change. Each paired server's sawe instance is unaware of the multi-server client. The same `remote.*` API still works.

### H. Out of scope (R-6c-push)

- FCM push notifications (separate phase R-6c-push when user wants it).
- Background reconnect to non-active servers ("ping all servers periodically to know who's online without switching to them"). Defer.
- Cloud sync of pairing entries across the user's own phones. Defer (probably never — pairing secrets are device-bound by design).

### I. Tests

`:core` doesn't change. `:app` doesn't currently have a test source set; manual smoke is the verification path. Document this clearly.

If the sub-agent wants to add minimal repository tests via Robolectric, that's a follow-up phase — don't blow scope.

## Acceptance

```bash
cd /home/spk/.spk/sawe/solutions/spk-solutions/sawe-mobile
ANDROID_HOME=$HOME/Android/Sdk JAVA_HOME=$HOME/.jdks/temurin-21.0.10 ./gradlew :core:test :app:assembleDebug :app:assembleRelease --rerun-tasks 2>&1 | tee /tmp/r6c-multi.txt | tail -10
grep -E "BUILD SUCCESSFUL|FAILURE:" /tmp/r6c-multi.txt
ls -la app/build/outputs/apk/release/*.apk
```

- [ ] `:core:test` BUILD SUCCESSFUL — 87 tests preserved.
- [ ] `:app:assembleDebug` + `:app:assembleRelease` BUILD SUCCESSFUL.
- [ ] Release APK ≤ 2.5 MB (R-6e was 2.24).
- [ ] Migration from `pairing_url` (R-6b key) to `paired_servers_v2` runs once on first R-6c launch.
- [ ] Per-server scoping on drafts / queue / lastSeen / nav (keys include `<serverId>:`).
- [ ] Servers screen appears when 2+ servers paired; cold start with 1 server keeps R-6b auto-resume behavior.
- [ ] Removing the active server gracefully falls back to another server or `pairing` if none left.

## Commit message

Subject: `app: multi-server pairing + Servers list screen (R-6c-multi)`

Body: outline the PairedServer schema, the R-6b → R-6c key migration, the per-server scoping of drafts/queue/lastSeen/nav, the new ServersListScreen, the Settings + nav graph changes, the cold-start routing heuristic, and what's deferred to R-6c-push.

## Reporting back

≤400 words. Include:
- New sibling-repo commit SHA on top of `c7fbddc`.
- New release APK size.
- Whether the R-6b → R-6c migration ran cleanly in a manual or simulated test.
- Per-server scoping verification: did you also scope the connection state banner / any other UI cache?
- Any nav-graph quirk with the `servers` ↔ `solutions` routing (especially around startDestination switching by paired-server count).
- Whether you added Robolectric or skipped `:app` tests (your call).
