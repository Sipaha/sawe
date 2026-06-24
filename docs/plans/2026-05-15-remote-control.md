# Remote Control panel + Android client

**Status:** scoping — needs decomposition into sub-phases before dispatch
**Goal:** Let the user remotely manage running Solutions and their agent
sessions from a mobile device — so they can give commands and watch
progress when away from the workstation.

## Context (user ask, 2026-05-15)

> Где-нибудь в нижней статус панели справа нужно сделать кнопку для
> открытия Remote Control панели + индикатор того, запущен сервер или
> нет. На панели Remote Control должны быть:
>
> 1. Адрес сервера (по умолчанию пустое с кнопкой "Вычислить через
>    [популярный сервис для получения своего IP]")
> 2. Порт (дефолтный, изменяемый)
> 3. Кнопка вкл/выкл
> 4. Список авторизованных клиентов и возможность добавить нового по
>    имени. У каждого клиента кнопка для показа QR кода: a) адрес+порт
>    сервера, б) секретный ключ (шифрование канала + аутентификация
>    сервером).
>
> Когда пользователь запускает сервер — стартуем сокет, слушаем
> подключения извне. Протокол — на твоё усмотрение.
>
> Суть сервера: доступ ко всем открытым solutions + возможность открыть
> новые. Главное внутри — доступ к диалогам с агентами (раздавать
> команды + следить за прогрессом, когда не у компа).
>
> Также нужно создать проект `sawe-mobile` для клиента,
> чтобы с Android-телефона управлять и следить за прогрессом агентов в
> солюшенах.

## Scope decomposition (proposed phases)

This is a multi-week feature; **needs 4–6 separate HEAVY phases**, each
with its own plan doc, each dispatched separately. This file is a stub /
scoping notes; the per-phase plan docs land as the phases start.

### Phase R-1 — Remote Control settings + status-bar widget

- Status-bar segment (right side of bottom panel): "Remote Control"
  button + on/off LED.
- Click opens the Remote Control modal.
- Modal: address (text field + "Detect via ifconfig.me" or similar
  button), port (default suggestion + editable), start/stop button,
  client list (initially empty).
- Settings persistence: `~/.config/sawe/remote-control.json` (or
  reuse SolutionsDb).
- No network listener yet — UI scaffolding only.

### Phase R-2 — Server protocol + listener

- Choose protocol: WebSocket (HTTP upgrade-friendly + proxiable) or raw
  TCP + length-prefixed JSON. Likely WebSocket since it'll work through
  most home routers + JS-friendly for the Android side.
- TLS / encryption: Noise Protocol over WebSocket (simple, well-vetted)
  OR JWT-style HMAC for message signing + TLS via Rustls. Pick at the
  start of this phase.
- Client auth: each authorized client has a name + a 256-bit shared
  secret (generated server-side, surfaced via QR code in R-3).
- MCP-protocol compatibility: probably expose a subset of the existing
  embedded MCP tool catalog (60 tools) as the remote API. Keeps
  client-side simple — they get the same surface as autonomous agents.
- Listener binds when user toggles ON; tears down on toggle OFF.
- Single-instance concurrency: at most N remote clients at once (config
  cap, default maybe 4).

### Phase R-3 — Authorized clients + QR codes

- "Add client" flow: name + auto-generate secret. Persisted alongside
  settings.
- QR code rendering (use `qrcode` crate) embedding:
  `spk-remote://<address>:<port>?secret=<base64>&client=<name>`.
- "Show QR" button per client → modal with the rendered PNG.
- Revoke flow.

### Phase R-4 — Solution + agent-session remote API surface

- New MCP-equivalent namespace `remote.*` (or reuse `solutions.*` +
  `solution_agent.*` with auth-gating). Lets a remote client:
  - List open solutions / get current state.
  - Open / close solutions.
  - List agent sessions per solution.
  - Send messages to an agent session.
  - Subscribe to agent_session_message_appended events (push).
- Auth: every request signed/authenticated by the client's secret.
- Throttling: rate limits per client.

### Phase R-5 — Android client scaffold (`sawe-mobile`)

- New project, OUTSIDE the sawe repo (or as a sibling crate? unlikely —
  Android needs Gradle/Kotlin, not Cargo; separate repo).
- QR scanner → parse `spk-remote://` URL → establish encrypted channel.
- UI: list of solutions, drill into one, list agent sessions, drill into
  one, chat UI to send messages + see streaming responses.
- Built with Jetpack Compose (UI) + OkHttp / Ktor (WebSocket client) +
  zxing or CameraX (QR).

### Phase R-6 — Android client polish

- Push notifications when agent finishes a turn (needs FCM or local
  notification on incoming WebSocket frame).
- Reconnect handling.
- Multiple-server support (one app, multiple workstation sessions).

## Dependencies between phases

- R-1 has none — pure local UI.
- R-2 depends on R-1 (settings + ON/OFF state).
- R-3 depends on R-1 (client list UI) + R-2 (secret generation tied to
  protocol).
- R-4 depends on R-2 (protocol foundation).
- R-5 depends on R-2 + R-3 + R-4 (entire server side first).
- R-6 depends on R-5.

Natural ordering: **R-1 → R-2 → R-4 → R-3 → R-5 → R-6**. R-3 can swap
with R-4 if the team wants to test QR provisioning end-to-end before
the full API surface lands.

## Out of scope (across this whole arc)

- iOS client. Android is the requested target.
- Self-hosted relay server for clients behind NAT — initial scope is
  same-network or router-port-forwarded only.
- Multi-user collaboration on the same solution from multiple remotes.
- Voice control.

## Open design questions (to resolve before R-2)

- **Protocol choice** — WebSocket+Noise vs raw TLS+TCP. Lean WebSocket
  for proxy traversal and JS-friendly tooling. Doesn't have to be
  decided right at R-1; firm up at start of R-2.
- **NAT traversal** — manual port forwarding instructions only, or
  bundle UPnP-auto-forward? Bundle uPnP if the crate ecosystem has a
  maintained option (`igd-next` etc.); otherwise document manual.
- **MCP-subset vs custom protocol** — does the remote client get the
  same 60-tool surface as an autonomous agent (powerful + already
  documented), or a curated subset (simpler + fewer footguns)? Likely
  curated for now; reuse the tool types but expose only the agent-
  session manipulation set.
- **Shared-secret vs proper PKI** — start with shared secret per
  client; PKI is out of scope.

## When the whole arc closes

The user can install the Android app, point it at a started workstation
server via QR, see their open solutions, open one, see its agent
sessions, send a message to an agent, and watch the agent's response
stream in. All over an encrypted channel authenticated by a per-client
secret.

---

**Next action:** when the supervisor reaches this in the pool, write the
**R-1 plan doc** (small, ~150 lines) and dispatch. R-1 is pure local
UI — no protocol decisions needed. The remaining phases unfold as
each predecessor completes.
