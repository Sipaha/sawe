use anyhow::Result;
use base64::Engine as _;
use gpui::{
    Context, DismissEvent, EventEmitter, FocusHandle, Focusable, IntoElement, KeyContext,
    ParentElement, Render, SharedString, Styled, Window, div, px,
};
use qrcodegen::{QrCode, QrCodeEcc};
use ui::{Button, ButtonStyle, Color, Label, LabelSize, prelude::*};
use workspace::ModalView;

/// Width (and height) of a single dark module in the rendered QR, in
/// logical pixels. ~6 px gives a 150-250 px QR for typical secret URLs
/// (QR module counts: 21 for v1 → 41 for v6 at Medium ECC) — large
/// enough for a phone camera to focus on, small enough to fit the
/// modal next to the URL.
const MODULE_PX: f32 = 6.0;

/// White quiet zone around the QR, in *modules*. The QR spec mandates
/// at least 4; we render the modal background white over the same
/// region, so a smaller padding works but 2 keeps the QR visually
/// crisp against the rest of the modal.
const QUIET_ZONE_MODULES: i32 = 2;

/// Workspace modal that displays the URL + QR for a freshly-added
/// authorized client. Re-mountable: closing this modal returns the user
/// to the main Remote Control list.
pub struct QrPopover {
    client_name: SharedString,
    /// Pre-built URL (or `None` when the inputs were insufficient — see
    /// `placeholder_reason`). Kept on the struct so we don't re-encode
    /// on every re-render.
    url: Option<SharedString>,
    code: Option<QrCode>,
    /// Human-readable explanation for *why* we don't have a QR to show.
    /// Populated when `url`/`code` are `None`.
    placeholder_reason: Option<SharedString>,
    focus_handle: FocusHandle,
}

impl QrPopover {
    pub fn new(
        client_name: SharedString,
        secret_standard_base64: String,
        address: Option<String>,
        port: u16,
        server_fingerprint: Option<[u8; 32]>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let address_trimmed = address
            .as_deref()
            .map(str::trim)
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        let placeholder_reason: Option<SharedString> = if address_trimmed.is_none() {
            Some("Set the server address first.".into())
        } else if port == 0 {
            Some("Set a non-zero server port first.".into())
        } else if server_fingerprint.is_none() {
            // Without the cert fingerprint the Android client has nothing
            // to pin the TLS handshake against. Force the user to toggle
            // Remote Control ON first (which materialises the cert and
            // exposes its SHA-256 via store.cert_fingerprint()).
            Some("Enable Remote Control first so the server cert is generated.".into())
        } else {
            None
        };

        let (url, code) = if placeholder_reason.is_none() {
            build_url_and_code(
                client_name.as_ref(),
                &secret_standard_base64,
                address_trimmed.as_deref(),
                port,
                server_fingerprint,
            )
        } else {
            (None, None)
        };
        Self {
            client_name,
            url,
            code,
            placeholder_reason,
            focus_handle,
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

/// Build the QR URL + matching `QrCode`. Returns `(None, None)` when the
/// inputs are too incomplete to produce a valid URL — the caller renders
/// a "set address first" message in that case.
fn build_url_and_code(
    client_name: &str,
    secret_standard_base64: &str,
    address: Option<&str>,
    port: u16,
    server_fingerprint: Option<[u8; 32]>,
) -> (Option<SharedString>, Option<QrCode>) {
    match build_url(
        client_name,
        secret_standard_base64,
        address,
        port,
        server_fingerprint,
    ) {
        Ok(url) => {
            // `QrCodeEcc::Medium` tolerates ~15% damage — comfortable
            // for a screen-captured-then-printed code while keeping the
            // module count low for short URLs.
            match QrCode::encode_text(&url, QrCodeEcc::Medium) {
                Ok(code) => (Some(SharedString::from(url)), Some(code)),
                Err(err) => {
                    log::warn!("remote_control: QR encode failed: {err}");
                    (Some(SharedString::from(url)), None)
                }
            }
        }
        Err(_) => (None, None),
    }
}

/// Construct the `sawe-remote://…` URL the Android client decodes.
/// The secret IS base64 (StandardChars include `/` and `+`) — those break
/// URL parsing, so we re-encode URL-SAFE for the `secret` query
/// parameter. The client name is percent-encoded. `server_fingerprint`,
/// when present, is the SHA-256 of the live cert DER from
/// `RemoteControlStore::cert_fingerprint`; it lands in the URL as
/// `&server_fp=<URL_SAFE_NO_PAD-base64>` so the Android client can pin
/// the TLS handshake against an exact known cert (ADR-0003).
fn build_url(
    client_name: &str,
    secret_standard_base64: &str,
    address: Option<&str>,
    port: u16,
    server_fingerprint: Option<[u8; 32]>,
) -> Result<String> {
    let address = address
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow::anyhow!("server address is required"))?;
    if port == 0 {
        anyhow::bail!("port must be non-zero");
    }
    let raw_secret = base64::engine::general_purpose::STANDARD
        .decode(secret_standard_base64.as_bytes())
        .map_err(|err| anyhow::anyhow!("stored secret is not valid base64: {err}"))?;
    let url_safe_secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw_secret);
    let encoded_name = urlencoding::encode(client_name);
    let mut url = format!(
        "sawe-remote://{address}:{port}?secret={url_safe_secret}&client={encoded_name}"
    );
    if let Some(fp) = server_fingerprint {
        let fp_url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(fp);
        url.push_str("&server_fp=");
        url.push_str(&fp_url_safe);
    }
    Ok(url)
}

impl EventEmitter<DismissEvent> for QrPopover {}
impl ModalView for QrPopover {}
impl Focusable for QrPopover {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QrPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = h_flex()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .gap_2()
            .child(Label::new("Pair client").size(LabelSize::Large))
            .child(
                Label::new(self.client_name.clone())
                    .size(LabelSize::Default)
                    .color(Color::Muted),
            );

        let footer = h_flex()
            .justify_end()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Button::new("qr-popover-close", "Close")
                    .style(ButtonStyle::Filled)
                    .on_click(cx.listener(|this, _, _window, cx| this.cancel(cx))),
            );

        // Determine body content: either an instructional message
        // (incomplete inputs) or the actual QR + URL.
        let body = if self.code.is_some() {
            self.render_qr_body(cx)
        } else {
            self.render_placeholder_body(cx)
        };

        v_flex()
            .key_context({
                let mut kc = KeyContext::new_with_defaults();
                kc.add("QrPopover");
                kc
            })
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(420.))
            .overflow_hidden()
            .on_action(cx.listener(|this, _: &menu::Cancel, _window, cx| this.cancel(cx)))
            .child(header)
            .child(body)
            .child(footer)
    }
}

impl QrPopover {
    fn render_qr_body(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let url = self.url.clone().unwrap_or_else(|| SharedString::from(""));
        let mut body = v_flex().id("qr-popover-body").p_3().gap_3().child(
            Label::new(format!(
                "Scan with the SPK Remote app to pair {}.",
                self.client_name
            ))
            .size(LabelSize::Small)
            .color(Color::Muted),
        );

        if let Some(code) = self.code.as_ref() {
            body = body.child(h_flex().justify_center().child(render_qr(code)));
        }

        body.child(
            v_flex()
                .gap_1()
                .child(Label::new("URL").size(LabelSize::Small))
                .child(
                    div()
                        .border_1()
                        .rounded_md()
                        .border_color(cx.theme().colors().border)
                        .px_2()
                        .py_1()
                        .child(Label::new(url).size(LabelSize::XSmall).buffer_font(cx)),
                ),
        )
    }

    fn render_placeholder_body(&self, _cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let message = self
            .placeholder_reason
            .clone()
            .unwrap_or_else(|| "Couldn't build a QR for this client.".into());
        v_flex().id("qr-popover-placeholder").p_4().child(
            Label::new(message)
                .color(Color::Muted)
                .size(LabelSize::Default),
        )
    }
}

/// Render the QR as a grid of `MODULE_PX`-sized squares. We deliberately
/// don't use GPUI's `svg()` element here — it only loads SVGs from a
/// file path / asset, not from an in-memory string, so we'd have to
/// write a temp file every re-render. A `div`-per-module grid is the
/// idiomatic GPUI fit.
fn render_qr(code: &QrCode) -> gpui::Div {
    let module_count = code.size();
    let padded = module_count + 2 * QUIET_ZONE_MODULES;
    let total_px = px(padded as f32 * MODULE_PX);

    let mut canvas = div()
        .w(total_px)
        .h(total_px)
        .bg(gpui::white())
        .flex()
        .flex_col();

    for y in -QUIET_ZONE_MODULES..(module_count + QUIET_ZONE_MODULES) {
        let mut row = div().w(total_px).h(px(MODULE_PX)).flex().flex_row();
        for x in -QUIET_ZONE_MODULES..(module_count + QUIET_ZONE_MODULES) {
            let cell = div().w(px(MODULE_PX)).h(px(MODULE_PX));
            // `get_module` is bounds-checked at the type level: out-of-range
            // returns false, but we treat the quiet zone explicitly to
            // make the QR's contract obvious to a future reader.
            let dark = (0..module_count).contains(&x)
                && (0..module_count).contains(&y)
                && code.get_module(x, y);
            let cell = if dark {
                cell.bg(gpui::black())
            } else {
                cell.bg(gpui::white())
            };
            row = row.child(cell);
        }
        canvas = canvas.child(row);
    }
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Test fixture: a deterministic 32-byte secret base64-encoded.
    fn fixture_secret() -> String {
        let raw = [0x10u8; 32];
        base64::engine::general_purpose::STANDARD.encode(raw)
    }

    /// Test fixture: a deterministic 32-byte cert fingerprint.
    fn fixture_fingerprint() -> [u8; 32] {
        [0xA5u8; 32]
    }

    #[test]
    fn url_contains_address_port_and_url_safe_secret() {
        let url = build_url(
            "Phone",
            &fixture_secret(),
            Some("203.0.113.1"),
            7777,
            Some(fixture_fingerprint()),
        )
        .expect("build_url");
        assert!(
            url.starts_with("sawe-remote://203.0.113.1:7777"),
            "got {url}"
        );
        assert!(url.contains("client=Phone"), "got {url}");
        // The URL-safe alphabet must not contain `+` or `/` (those are
        // reserved in standard base64 and would break URL parsing on
        // the Android side).
        let query = url.split_once('?').expect("has query").1;
        let secret_param = query
            .split('&')
            .find_map(|p| p.strip_prefix("secret="))
            .expect("has secret");
        assert!(
            !secret_param.contains('+') && !secret_param.contains('/'),
            "secret param must be URL-safe base64 (no `+` or `/`): {secret_param}"
        );
    }

    #[test]
    fn url_safe_secret_round_trips_back_to_original_bytes() {
        let raw = [0xABu8; 32];
        let standard = base64::engine::general_purpose::STANDARD.encode(raw);
        let url = build_url(
            "X",
            &standard,
            Some("1.2.3.4"),
            7777,
            Some(fixture_fingerprint()),
        )
        .expect("build_url");

        let secret_param = url
            .split('?')
            .nth(1)
            .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("secret=")))
            .expect("has secret");
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(secret_param.as_bytes())
            .expect("url-safe decode");
        assert_eq!(decoded, raw, "round-tripped secret must equal original");
    }

    #[test]
    fn url_encodes_client_name_with_special_chars() {
        let url = build_url(
            "My Phone & Tablet",
            &fixture_secret(),
            Some("198.51.100.5"),
            8080,
            Some(fixture_fingerprint()),
        )
        .expect("build_url");
        // Space → %20, & → %26.
        assert!(
            url.contains("client=My%20Phone%20%26%20Tablet"),
            "got {url}"
        );
    }

    #[test]
    fn url_carries_server_fp_url_safe_base64() {
        // Fingerprint bytes containing all-FF would round-trip through
        // standard base64 as `////////...` — the URL-safe alphabet must
        // produce `____...` instead. Verifies the encode path is the
        // URL-safe one, not the default.
        let fp = [0xFFu8; 32];
        let url = build_url(
            "Phone",
            &fixture_secret(),
            Some("203.0.113.1"),
            7777,
            Some(fp),
        )
        .expect("build_url");
        let fp_param = url
            .split_once('?')
            .expect("has query")
            .1
            .split('&')
            .find_map(|p| p.strip_prefix("server_fp="))
            .expect("server_fp param present");
        assert!(
            !fp_param.contains('+') && !fp_param.contains('/') && !fp_param.contains('='),
            "fingerprint param must be URL-safe base64 with no padding: {fp_param}"
        );
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(fp_param.as_bytes())
            .expect("decode url-safe");
        assert_eq!(
            decoded, fp,
            "fingerprint round-trips back to original 32 bytes"
        );
    }

    #[test]
    fn url_omits_server_fp_when_fingerprint_missing() {
        // R-2 ships a placeholder when the cert isn't ready, but
        // `build_url` itself stays composable: no fingerprint → no
        // server_fp param (callers above this layer guard the
        // user-facing flow).
        let url = build_url("Phone", &fixture_secret(), Some("203.0.113.1"), 7777, None)
            .expect("build_url");
        assert!(!url.contains("server_fp="), "got {url}");
    }

    #[test]
    fn build_url_rejects_missing_address() {
        assert!(
            build_url(
                "Phone",
                &fixture_secret(),
                None,
                7777,
                Some(fixture_fingerprint())
            )
            .is_err()
        );
        assert!(
            build_url(
                "Phone",
                &fixture_secret(),
                Some(""),
                7777,
                Some(fixture_fingerprint())
            )
            .is_err()
        );
        assert!(
            build_url(
                "Phone",
                &fixture_secret(),
                Some("   "),
                7777,
                Some(fixture_fingerprint())
            )
            .is_err()
        );
    }

    #[test]
    fn build_url_rejects_zero_port() {
        assert!(
            build_url(
                "Phone",
                &fixture_secret(),
                Some("203.0.113.1"),
                0,
                Some(fixture_fingerprint())
            )
            .is_err()
        );
    }

    #[test]
    fn qr_encode_succeeds_for_built_url() {
        let url = build_url(
            "Phone",
            &fixture_secret(),
            Some("203.0.113.1"),
            7777,
            Some(fixture_fingerprint()),
        )
        .expect("build_url");
        let code = QrCode::encode_text(&url, QrCodeEcc::Medium).expect("encode");
        assert!(code.size() > 0, "QR must have non-zero size");
        // Module-presence isn't deterministic across qrcodegen versions
        // (mask selection depends on the encoded content), but the
        // grid must contain *some* dark modules — the corners alone
        // are 3 fixed finder patterns with 7×7 dark blocks.
        let mut dark_count = 0;
        for y in 0..code.size() {
            for x in 0..code.size() {
                if code.get_module(x, y) {
                    dark_count += 1;
                }
            }
        }
        assert!(dark_count > 0, "QR must have at least one dark module");
    }

    #[test]
    fn placeholder_used_when_address_missing() {
        let (url, code) = build_url_and_code(
            "Phone",
            &fixture_secret(),
            None,
            7777,
            Some(fixture_fingerprint()),
        );
        assert!(url.is_none() && code.is_none());
    }

    #[test]
    fn placeholder_used_when_port_zero() {
        let (url, code) = build_url_and_code(
            "Phone",
            &fixture_secret(),
            Some("203.0.113.1"),
            0,
            Some(fixture_fingerprint()),
        );
        assert!(url.is_none() && code.is_none());
    }
}
