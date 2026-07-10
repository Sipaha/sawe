//! Image decode/preview cluster for the Solution conversation view.
//!
//! Relocated verbatim from `conversation_render.rs` (Tier-1 god-object split).

use super::*;

/// `[image #N]` placeholder pattern injected by the compose paste
/// handler. The capture group is the 1-based image index. Used by
/// the recall path (`session_view::recall`) where we want ONLY the
/// desktop-typed placeholders, not the `\`Image\`` literals emitted
/// by acp_thread's image-chunk merge — those don't carry a recall
/// label and would just confuse the recall surface.
pub(crate) static IMAGE_PLACEHOLDER_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[image #(\d+)\]").expect("static regex compiles")
    });

/// Combined regex for [clean_user_message_text]: matches either the
/// desktop-paste `[image #N]` placeholder OR the literal `\`Image\``
/// inline-code marker that `acp_thread::ContentBlock::append` emits
/// when merging an image chunk into a multi-block user message
/// (e.g. mobile-originated text + attachment bundle). The capture
/// group is the digits inside `[image #N]` when that variant matched;
/// `None` when the `\`Image\`` branch matched, in which case the
/// caller synthesises a 1-based ordinal from the match position.
pub(crate) static USER_IMAGE_PLACEHOLDER_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[image #(\d+)\]|`Image`").expect("static regex compiles")
    });

/// The bare `\`Image\`` literal that `acp_thread::ContentBlock::append` emits
/// for an image chunk. Used by [clean_user_message_text] to STRIP these
/// redundant chunk-literals when the message also carries explicit
/// `[image #N]` paste-placeholders (which already represent the same images),
/// so a desktop-composed attachment doesn't render as two links.
pub(crate) static IMAGE_LITERAL_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"`Image`").expect("static regex compiles"));

/// Rewrite the `\`Image\`` literals an assistant `to_markdown` emits for
/// agent-emitted image content blocks (Anthropic `image` blocks routed
/// through `acp_thread::ContentBlock::Image`) into `spk-image://N` markdown
/// links so the same render path the user-attached images use can pop a
/// fullscreen preview. `image_index_base` is the GLOBAL image cursor at
/// the start of this entry — `summarize_entry` advances it once per entry
/// so the indices stay aligned with `EntryImage.index` in the wire
/// `images` array (each `Image` block consumes one slot in cursor order).
/// Mobile already handles the `spk-image://N` scheme for user-attached
/// images (`SessionDetailScreen.kt::onLinkClick`); reusing it for agent
/// images means a single render path covers both sides.
///
/// Pass-through for entries without `\`Image\`` literals (the common
/// shape — most assistant messages are pure text/tool_use, no image
/// chunks). The `## Assistant` header and any `<thinking>` blocks are
/// preserved verbatim; this is purely an image-link rewrite.
pub(crate) fn clean_assistant_message_text(text: &str, image_index_base: usize) -> String {
    if !IMAGE_LITERAL_RE.is_match(text) {
        return text.to_string();
    }
    let mut local: usize = 0;
    IMAGE_LITERAL_RE
        .replace_all(text, |_caps: &regex::Captures| {
            let idx = image_index_base + local;
            local += 1;
            // `[image #N]` label uses the 1-based local ordinal so the
            // visible text in the bubble counts up per-message (`image
            // #1`, `image #2`, …) rather than exposing the global cursor
            // (`spk-image://7`, `spk-image://8`, …). The bracket inner
            // text is purely user-facing; the URL drives the click.
            format!("[image #{}](spk-image://{idx})", local)
        })
        .into_owned()
}

/// Mirrors `acp_thread::ContentBlock::decode_image` (private upstream)
/// so we can re-decode image chunks at render time without exposing a
/// new `pub` surface in the acp_thread crate. Returns None on malformed
/// base64 / unsupported mime — caller falls back to the placeholder.
pub(crate) fn decode_image_local(
    image_content: &acp::ImageContent,
) -> Option<std::sync::Arc<gpui::Image>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image_content.data.as_bytes())
        .ok()?;
    let format = gpui::ImageFormat::from_mime_type(&image_content.mime_type)?;
    Some(std::sync::Arc::new(gpui::Image::from_bytes(format, bytes)))
}

/// Opens the given image in a centred OS popup window for full-size
/// inspection. Used by the chat thumbnail click handler.
pub(crate) fn open_image_preview(
    image: std::sync::Arc<gpui::Image>,
    window: &mut Window,
    cx: &mut App,
) {
    let display_size = window
        .display(cx)
        .or_else(|| cx.primary_display())
        .map(|d| d.bounds().size)
        .unwrap_or(gpui::Size {
            width: px(800.0),
            height: px(600.0),
        });
    let size = gpui::Size {
        width: display_size.width * 0.6,
        height: display_size.height * 0.7,
    };
    let bounds = gpui::WindowBounds::centered(size, cx);
    if let Err(err) = cx.open_window(
        gpui::WindowOptions {
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Image preview".into()),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            window_bounds: Some(bounds),
            is_resizable: true,
            is_minimizable: true,
            kind: gpui::WindowKind::Normal,
            ..Default::default()
        },
        move |window, cx| {
            window.activate_window();
            cx.new(|_| ImagePreviewWindowView { image })
        },
    ) {
        log::error!("failed to open image preview window: {err:?}");
    }
}

pub(crate) struct ImagePreviewWindowView {
    image: std::sync::Arc<gpui::Image>,
}

impl Render for ImagePreviewWindowView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .flex()
            .items_center()
            .justify_center()
            .child(
                gpui::img(self.image.clone())
                    .object_fit(gpui::ObjectFit::Contain)
                    .size_full(),
            )
    }
}
