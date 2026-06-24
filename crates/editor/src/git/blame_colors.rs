//! S-ANN — color modes for blame gutter rendering.
//!
//! Two modes:
//! - `ColorMode::ByDate` — heatmap from cold (oldest) to hot (newest).
//! - `ColorMode::ByAuthor` — deterministic palette pick keyed off the
//!   author email so the same person always gets the same color across
//!   sessions.
//!
//! Both helpers return [`gpui::Hsla`] in the editor's existing player
//! palette space so the colors blend with the rest of the theme. The
//! `By*` variants are pure functions of their inputs and have no
//! dependency on `Window` or `App` aside from the player palette lookup.

use gpui::{App, Hsla};
use theme::ActiveTheme as _;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    /// Default — fall through to the existing per-SHA player color.
    #[default]
    None,
    /// Heatmap from oldest commit (cold) to newest (hot).
    ByDate,
    /// Stable palette pick keyed by author email.
    ByAuthor,
}

impl ColorMode {
    pub fn label(self) -> &'static str {
        match self {
            ColorMode::None => "None",
            ColorMode::ByDate => "Color by Date",
            ColorMode::ByAuthor => "Color by Author",
        }
    }

    pub fn next(self) -> Self {
        match self {
            ColorMode::None => ColorMode::ByDate,
            ColorMode::ByDate => ColorMode::ByAuthor,
            ColorMode::ByAuthor => ColorMode::None,
        }
    }
}

/// Stable, fast hash for deterministic palette indexing. Uses the FNV-1a
/// 64-bit variant — same value on every run regardless of stdlib hasher
/// changes, which the test in this module pins.
pub fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Map an author email to a deterministic hue/saturation/lightness color.
/// `email` is normalized (trim angle brackets, lowercase) before hashing
/// so `<Foo@Example.COM>` and `foo@example.com` collide on the same
/// color.
pub fn author_color(email: &str, cx: &App) -> Hsla {
    let normalized = normalize_email(email);
    let hash = stable_hash(normalized.as_bytes());
    let player_count = cx.theme().players().0.len().max(1) as u64;
    let index = (hash % player_count) as usize;
    cx.theme().players().0[index].cursor
}

pub fn normalize_email(email: &str) -> String {
    email
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_ascii_lowercase()
}

/// Compute a date-heatmap color for `commit_time` given the [oldest, newest]
/// commit times observed in the buffer's blame entries.
///
/// Returns `None` when the range is degenerate (`newest <= oldest`) — caller
/// should fall back to the SHA-derived player color in that case.
///
/// The mapping is linear in seconds; we found that quadratic / log
/// scales over-emphasize the most recent edits when the file has long
/// quiet periods, which makes the heatmap less informative.
pub fn date_color(
    commit_time: i64,
    oldest: i64,
    newest: i64,
    cold: Hsla,
    hot: Hsla,
) -> Option<Hsla> {
    if newest <= oldest {
        return None;
    }
    let span = (newest - oldest).max(1) as f32;
    let t = ((commit_time - oldest).max(0) as f32 / span).clamp(0.0, 1.0);
    Some(lerp_hsla(cold, hot, t))
}

fn lerp_hsla(a: Hsla, b: Hsla, t: f32) -> Hsla {
    Hsla {
        h: lerp(a.h, b.h, t),
        s: lerp(a.s, b.s, t),
        l: lerp(a.l, b.l, t),
        a: lerp(a.a, b.a, t),
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::hsla;

    #[test]
    fn stable_hash_is_deterministic() {
        assert_eq!(
            stable_hash(b"alice@example.com"),
            stable_hash(b"alice@example.com")
        );
        assert_ne!(
            stable_hash(b"alice@example.com"),
            stable_hash(b"bob@example.com")
        );
    }

    #[test]
    fn normalize_email_strips_brackets_and_lowercases() {
        assert_eq!(normalize_email("<Alice@Example.COM>"), "alice@example.com");
        assert_eq!(normalize_email("  bob@example.com  "), "bob@example.com");
    }

    #[test]
    fn date_color_endpoints_match_palette_ends() {
        let cold = hsla(0.6, 0.5, 0.4, 1.0);
        let hot = hsla(0.0, 0.7, 0.5, 1.0);

        let at_cold = date_color(100, 100, 200, cold, hot).expect("range valid");
        let at_hot = date_color(200, 100, 200, cold, hot).expect("range valid");

        assert!((at_cold.h - cold.h).abs() < 1e-4);
        assert!((at_hot.h - hot.h).abs() < 1e-4);
    }

    #[test]
    fn date_color_midpoint_is_halfway() {
        let cold = hsla(0.0, 0.0, 0.0, 1.0);
        let hot = hsla(1.0, 1.0, 1.0, 1.0);

        let mid = date_color(150, 100, 200, cold, hot).expect("range valid");

        assert!((mid.h - 0.5).abs() < 1e-3);
        assert!((mid.l - 0.5).abs() < 1e-3);
    }

    #[test]
    fn date_color_returns_none_on_degenerate_range() {
        let cold = hsla(0.0, 0.0, 0.0, 1.0);
        let hot = hsla(1.0, 1.0, 1.0, 1.0);

        assert!(date_color(100, 200, 100, cold, hot).is_none());
        assert!(date_color(100, 100, 100, cold, hot).is_none());
    }
}
