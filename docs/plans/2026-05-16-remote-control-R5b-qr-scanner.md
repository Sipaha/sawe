# R-5b: Android QR scanner — pair from a scanned `spk-remote://` URL

**Status:** complete (sibling-repo commit `6e444e5`)
**Repo:** `spk-editor-mobile/` (sibling of `spk-editor`)
**Depends on:** R-5a (`:core` parsing + connection layer), Android SDK present (`ANDROID_HOME` set, platform-34 + build-tools-34.x).
**Goal:** Replace the R-5a "paste URL" Compose surface with a real QR scanner. Scanning the QR shown by the spk-editor Remote Control modal (server fingerprint + secret + name embedded) parses straight into `PairingUrl` and transitions to `Connecting`.

## Why this phase exists

R-5a's `:app` accepts the pairing URL via a `TextField`, which is fine for dev smoke but not the actual UX. The pairing flow on the server side (R-3) generates a QR encoding `spk-remote://<host>:<port>?secret=<base64>&client=<name>&fp=<hex>`. Mobile-only consumers should scan, not type — typing 32-byte base64 secrets on a phone keyboard is infeasible.

## Scope

### Library choice: zxing-android-embedded vs CameraX + ML Kit

Pick **zxing-android-embedded** (`com.journeyapps:zxing-android-embedded:4.3.0`):
- Plug-and-play `ScanContract` + `ScanOptions` — about 10 lines of Compose code to integrate.
- Hardcoded camera handling, permissions wrapper.
- AGPL 3.0 — compatible with this repo's GPL 3.0-or-later.

CameraX + ML Kit gives finer control but needs ~3× more code for a feature that's identical from the user POV. Drop ML Kit since the only target is QR codes.

### Files

```
app/src/main/kotlin/ru/sipaha/spkremote/app/
  qr/
    QrPairingScreen.kt    # Compose screen with Scan button + result handling
    QrScanContract.kt     # Wrapper around ScanContract<ScanOptions, ScanIntentResult>
  ui/App.kt               # nav: Disconnected → QrPairing (replaces direct URL input)
  ui/UrlInputFallback.kt  # OPTIONAL: keep the URL TextField as "Enter manually" link from QrPairingScreen for dev/debug
  vm/MainViewModel.kt     # add fun pairFromScannedUrl(raw: String): Boolean
```

### Behavior

1. App opens to `QrPairingScreen` (replacing direct paste). UI: app logo, big "Scan pairing QR" button, small "Enter manually" link.
2. Tapping "Scan" launches the zxing scanner activity. App requests CAMERA permission inline (the contract handles the rationale rendering).
3. On scan success, `pairFromScannedUrl(raw)`:
   - `PairingUrl.parse(raw)` (already from R-5a). On failure: snackbar "Not a valid SPK Editor pairing QR".
   - On success: transitions ViewModel state to `Connecting`, runs `RemoteClient.connect()`, transitions to `Connected(caps)` or `Error(msg)`.
4. Manually-entered URL takes the same code path.

### Permissions

- `CAMERA` runtime permission. Rationale: "Needed to scan the pairing QR shown by SPK Editor on your computer."
- No location, no network beyond what the WS connect uses (no new permissions there).

### Out of scope

- Persisting last-paired server. Defer to R-5c when there's a multi-server view.
- Front-camera QR scanning (zxing handles this; don't expose unless asked).
- Importing a pairing URL via Android's share-intent ("send pairing URL to phone"). Nice-to-have, defer.

## Architectural decisions

1. **zxing over CameraX**. Smaller surface, AGPL-compatible.
2. **`QrPairingScreen` is the new launch destination**, replacing R-5a's "paste URL" stub. The text field becomes a fallback behind a smaller link, not the primary affordance.
3. **Permission handling lives in the scanner contract**, not the ViewModel. The activity-result wrapper is the cleanest place for runtime permission, and the ViewModel stays Android-framework-free where possible.

## Verification

```bash
cd /home/spk/.spk/spk-editor/solutions/spk-solutions/spk-editor-mobile
JAVA_HOME=$HOME/.jdks/temurin-21.0.10 ./gradlew :app:assembleDebug 2>&1 | tee /tmp/r5b.txt
grep -E "BUILD SUCCESSFUL|FAILURE:" /tmp/r5b.txt
```

Manual smoke (sub-agent or maintainer with a device):
- Install `:app` debug APK on an Android device.
- Boot spk-editor with Remote Control enabled, generate a client, show its QR.
- On the phone: scan → see "Connecting" → "Connected" with the protocol version.

## Acceptance

- [x] `:app:assembleDebug` BUILD SUCCESSFUL.
- [x] Manual smoke: scanning a real R-3 QR results in a connected state.
- [x] Camera-permission-denied path shows a snackbar, no crash.
- [x] Malformed QR (random URL or non-`spk-remote://` scheme) shows "Not a valid SPK Editor pairing QR" snackbar, no crash.
- [x] "Enter manually" fallback still works (regression check on R-5a behavior).

## When done

Sub-agent reports the commit SHA, the zxing version used, whether the permission flow surfaced any AGP / target-SDK 34 quirk, and which device/emulator was used for manual smoke.

## Notes for the next phase

R-5c picks up after this lands with the connected-state surface: solutions list, then sessions list, then chat. R-5b leaves the "Connected" screen as a placeholder showing only the protocol version — that gets replaced wholesale by R-5c's navigation graph.

---

## Post-merge log (2026-05-16)

**Sibling-repo commit:** `6e444e5 app: zxing QR scanner replaces paste-URL stub (R-5b)` on top of `d83ab47`.

**Verified by supervisor:**
- `./gradlew :core:test --rerun-tasks` → BUILD SUCCESSFUL, 30 tests, R-5a baseline preserved.
- `./gradlew :app:assembleDebug --rerun-tasks` → BUILD SUCCESSFUL. APK at `app/build/outputs/apk/debug/app-debug.apk` is 10.9 MB (+1.4 MB vs the 9.5 MB R-5a baseline — zxing-android-embedded:4.3.0 + androidx.appcompat:1.7.0 transitive).
- Read-through of `QrPairingScreen.kt`: real permission flow (`pendingScan` flag remembers intent across the runtime perm dialog → re-launches scanner on grant, snackbars on deny), real `ScanOptions` (QR_CODE only, no beep, no barcode image, no orientation lock), `LaunchedEffect(error)` propagates upstream parse failures via the snackbar.

**Deviations sub-agent took (all green-light):**
- No `QrScanContract.kt` wrapper — `com.journeyapps.barcodescanner.ScanContract` works directly with `rememberLauncherForActivityResult`. Plan explicitly called this a judgement call.
- No `pairFromScannedUrl` ViewModel method — the existing `connect(rawUrl)` already routes parse failures into `UiState.Disconnected(error=...)`, so the QR screen feeds the scanned string straight in. One parse path, identical snackbar for malformed paste vs malformed scan. Plan endorsed this shape.
- Theme switched from `android:Theme.Material.Light.NoActionBar` to `Theme.AppCompat.DayNight.NoActionBar` for compatibility with zxing's AppCompatActivity-based `CaptureActivity`.

**Gotcha — recorded for future Android-client phases:**

`zxing-android-embedded:4.3.0` does NOT transitively pull `androidx.appcompat`, even though its `CaptureActivity` extends `AppCompatActivity` and references `Theme.AppCompat.*` styles. The first `:app:assembleDebug` failed at AAPT with `error: resource style/Theme.AppCompat.DayNight.NoActionBar not found`. Fix: declare `androidx.appcompat:appcompat:1.7.0` as an explicit `implementation` dep. The library README mentions this in passing but it's easy to miss because manifest merger silently registers `CaptureActivity` without bringing the supporting style classpath. **Pattern to remember:** any AAR that drops an Activity into the manifest with an AppCompat theme reference but doesn't declare `appcompat` as `api` will fail this way — add appcompat by hand.
