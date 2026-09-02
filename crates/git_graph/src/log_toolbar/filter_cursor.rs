//! Keyboard cursor shared by the three multi-select filter popovers in the
//! Git Graph log toolbar (branch / user / path).
//!
//! The three popovers keep genuinely different row types (`Row::Branch`,
//! `Row::Author`, `Row::Path`) and different selection keys, so the shared
//! piece is deliberately type-agnostic: it owns an index into the *filtered*
//! row list plus that list's scroll handle, and every "is this row a real item
//! or a group header" question is answered by a predicate the caller passes
//! in. Nothing here knows what a row is, so no common row abstraction is
//! forced onto the three views.
//!
//! The cursor is orthogonal to each popover's `selected` set: `selected` means
//! *checked* (what `Apply` will commit), the cursor means *where the keyboard
//! is* (what `menu::Confirm` will toggle).

use gpui::{ScrollStrategy, UniformListScrollHandle};

pub(super) struct FilterCursor {
    index: usize,
    scroll_handle: UniformListScrollHandle,
}

impl FilterCursor {
    pub(super) fn new() -> Self {
        Self {
            index: 0,
            scroll_handle: UniformListScrollHandle::new(),
        }
    }

    pub(super) fn index(&self) -> usize {
        self.index
    }

    pub(super) fn scroll_handle(&self) -> &UniformListScrollHandle {
        &self.scroll_handle
    }

    /// Park the cursor on the first actionable row.
    ///
    /// Called whenever the row list is rebuilt. After a query change the old
    /// index addresses a row that either no longer exists or is now a
    /// different entry entirely, so carrying it over would highlight an
    /// arbitrary item; landing on the first match is also what makes
    /// "type, then press Enter" act on the top hit.
    pub(super) fn reset(&mut self, row_count: usize, is_actionable: impl Fn(usize) -> bool) {
        self.index = (0..row_count).find(|ix| is_actionable(*ix)).unwrap_or(0);
        self.reveal();
    }

    /// Returns whether the cursor actually moved, so callers only `cx.notify()`
    /// on a real change.
    pub(super) fn move_to(&mut self, index: usize) -> bool {
        if self.index == index {
            return false;
        }
        self.index = index;
        self.reveal();
        true
    }

    pub(super) fn select_next(
        &mut self,
        row_count: usize,
        is_actionable: impl Fn(usize) -> bool,
    ) -> bool {
        match (self.index.saturating_add(1)..row_count).find(|ix| is_actionable(*ix)) {
            Some(index) => self.move_to(index),
            None => false,
        }
    }

    pub(super) fn select_previous(&mut self, is_actionable: impl Fn(usize) -> bool) -> bool {
        match (0..self.index).rev().find(|ix| is_actionable(*ix)) {
            Some(index) => self.move_to(index),
            None => false,
        }
    }

    pub(super) fn select_first(
        &mut self,
        row_count: usize,
        is_actionable: impl Fn(usize) -> bool,
    ) -> bool {
        match (0..row_count).find(|ix| is_actionable(*ix)) {
            Some(index) => self.move_to(index),
            None => false,
        }
    }

    pub(super) fn select_last(
        &mut self,
        row_count: usize,
        is_actionable: impl Fn(usize) -> bool,
    ) -> bool {
        match (0..row_count).rev().find(|ix| is_actionable(*ix)) {
            Some(index) => self.move_to(index),
            None => false,
        }
    }

    /// `Nearest` rather than `Center`: these lists are stepped through one row
    /// at a time with the arrow keys, and `Center` half-page-jumps the viewport
    /// the moment the cursor crosses an edge.
    fn reveal(&self) {
        self.scroll_handle
            .scroll_to_item(self.index, ScrollStrategy::Nearest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows 0 and 3 are group headers, 1/2/4 are items — the shape all three
    /// popovers build.
    fn actionable(index: usize) -> bool {
        matches!(index, 1 | 2 | 4)
    }

    #[test]
    fn test_reset_lands_on_first_actionable_row() {
        let mut cursor = FilterCursor::new();
        cursor.move_to(4);
        cursor.reset(5, actionable);
        assert_eq!(cursor.index(), 1);
    }

    #[test]
    fn test_reset_with_no_actionable_rows_parks_at_zero() {
        let mut cursor = FilterCursor::new();
        cursor.move_to(4);
        cursor.reset(0, actionable);
        assert_eq!(cursor.index(), 0);
    }

    #[test]
    fn test_select_next_skips_headers_and_stops_at_the_end() {
        let mut cursor = FilterCursor::new();
        cursor.reset(5, actionable);
        assert!(cursor.select_next(5, actionable));
        assert_eq!(cursor.index(), 2);
        assert!(cursor.select_next(5, actionable), "must skip header row 3");
        assert_eq!(cursor.index(), 4);
        assert!(
            !cursor.select_next(5, actionable),
            "the last actionable row is the end of the line"
        );
        assert_eq!(cursor.index(), 4);
    }

    #[test]
    fn test_select_previous_skips_headers_and_stops_at_the_start() {
        let mut cursor = FilterCursor::new();
        cursor.move_to(4);
        assert!(cursor.select_previous(actionable), "must skip header row 3");
        assert_eq!(cursor.index(), 2);
        assert!(cursor.select_previous(actionable));
        assert_eq!(cursor.index(), 1);
        assert!(
            !cursor.select_previous(actionable),
            "row 0 is a header, so there is nowhere above row 1 to go"
        );
        assert_eq!(cursor.index(), 1);
    }

    #[test]
    fn test_select_first_and_last() {
        let mut cursor = FilterCursor::new();
        cursor.move_to(2);
        assert!(cursor.select_last(5, actionable));
        assert_eq!(cursor.index(), 4);
        assert!(cursor.select_first(5, actionable));
        assert_eq!(cursor.index(), 1);
    }
}
