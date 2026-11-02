use super::*;

const MAX_HISTORY_ENTRIES: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq)]
struct HistoryEntry {
    position: ViewPosition,
    destination: Option<LinkDestination>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct JumpList {
    entries: Vec<HistoryEntry>,
    cursor: usize,
}

impl JumpList {
    fn push(&mut self, position: ViewPosition) {
        self.push_entry(HistoryEntry {
            position,
            destination: None,
        });
    }

    fn push_entry(&mut self, entry: HistoryEntry) {
        if !self.entries.is_empty() {
            self.entries.truncate(self.cursor + 1);
        }
        if self.entries.last() != Some(&entry) {
            if self.entries.len() == MAX_HISTORY_ENTRIES {
                self.entries.remove(0);
            }
            self.entries.push(entry);
        }
        self.cursor = self.entries.len().saturating_sub(1);
    }

    fn jump(
        &mut self,
        origin: ViewPosition,
        provisional: ViewPosition,
        destination: Option<LinkDestination>,
    ) {
        if self
            .entries
            .get(self.cursor)
            .is_none_or(|entry| entry.position != origin)
        {
            self.push(origin);
        }
        self.push_entry(HistoryEntry {
            position: provisional,
            destination,
        });
    }

    pub(super) fn update_tip(&mut self, position: ViewPosition) {
        let Some(entry) = self.entries.get_mut(self.cursor) else {
            return;
        };
        if entry.destination.take().is_some() {
            entry.position = position;
            self.deduplicate();
        }
    }

    fn pending_destination(&self) -> Option<LinkDestination> {
        self.entries
            .get(self.cursor)
            .and_then(|entry| entry.destination)
    }

    pub(super) fn clamp_pages(&mut self, pages: u32) {
        let max_page = pages.saturating_sub(1);
        for entry in &mut self.entries {
            entry.position.page = entry.position.page.min(max_page);
            if let Some(destination) = &mut entry.destination {
                destination.page = destination.page.min(max_page);
            }
        }
        self.deduplicate();
    }

    fn deduplicate(&mut self) {
        let cursor = self.cursor;
        let mut write = 0;
        for read in 0..self.entries.len() {
            if write == 0 || self.entries[read] != self.entries[write - 1] {
                self.entries[write] = self.entries[read];
                write += 1;
            }
            if read == cursor {
                self.cursor = write - 1;
            }
        }
        self.entries.truncate(write);
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
    }

    fn backward(&mut self, current: ViewPosition) -> Option<ViewPosition> {
        let entry = self.entries.get(self.cursor)?;
        if self.cursor + 1 == self.entries.len()
            && entry.destination.is_none()
            && entry.position != current
        {
            let target = entry.position;
            self.push(current);
            self.cursor = self.cursor.saturating_sub(1);
            return Some(target);
        }
        if self.cursor == 0 {
            return None;
        }
        self.cursor -= 1;
        Some(self.entries[self.cursor].position)
    }

    fn forward(&mut self) -> Option<ViewPosition> {
        if self.cursor + 1 >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        Some(self.entries[self.cursor].position)
    }
}

impl App {
    pub(super) fn follow_link(
        &mut self,
        target: LinkTarget,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        match target {
            LinkTarget::Internal {
                page,
                top_ratio,
                left_ratio,
            } => {
                let current = self.history_position();
                let page = page.min(self.tab().page_count.saturating_sub(1));
                let tab = self.tab_mut();
                tab.page = page;
                tab.center_content = true;
                if top_ratio.is_some() {
                    tab.scroll_y = 0;
                }
                tab.link_history.jump(
                    current,
                    ViewPosition {
                        page: tab.page,
                        scroll_x: tab.scroll_x,
                        scroll_y: tab.scroll_y,
                        center_content: tab.center_content,
                    },
                    Some(LinkDestination {
                        page,
                        top_ratio,
                        left_ratio,
                    }),
                );
                tab.pending_destination = Some(LinkDestination {
                    page,
                    top_ratio,
                    left_ratio,
                });
                self.request_current(output)?;
            }
            LinkTarget::Uri(uri) => {
                if uri.trim().is_empty() {
                    self.draw_status(output, self.viewport()?, "empty link")?;
                } else {
                    write_clipboard_osc52(output, &uri)?;
                    self.draw_status(output, self.viewport()?, "copied link to clipboard")?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn follow_link_back(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let current = self.history_position();
        let Some(previous) = self.tab_mut().link_history.backward(current) else {
            self.draw_status(output, self.viewport()?, "no link history")?;
            return Ok(());
        };
        self.restore_history_position(previous, output)
    }

    pub(super) fn follow_link_forward(&mut self, output: &mut impl Write) -> Result<(), AppError> {
        let Some(next) = self.tab_mut().link_history.forward() else {
            self.draw_status(output, self.viewport()?, "no forward link history")?;
            return Ok(());
        };
        self.restore_history_position(next, output)
    }

    pub(super) fn history_position(&self) -> ViewPosition {
        let tab = self.tab();
        ViewPosition {
            page: tab.page,
            scroll_x: tab.scroll_x,
            scroll_y: tab.scroll_y,
            center_content: tab.center_content,
        }
    }

    pub(super) fn update_history_tip(&mut self) {
        let position = self.history_position();
        self.tab_mut().link_history.update_tip(position);
    }

    fn restore_history_position(
        &mut self,
        position: ViewPosition,
        output: &mut impl Write,
    ) -> Result<(), AppError> {
        let tab = self.tab_mut();
        tab.page = position.page.min(tab.page_count.saturating_sub(1));
        tab.scroll_x = position.scroll_x;
        tab.scroll_y = position.scroll_y;
        tab.center_content = position.center_content;
        tab.pending_destination = tab.link_history.pending_destination();
        self.request_current(output)
    }
}

#[cfg(test)]
mod tests {
    use super::{JumpList, LinkDestination, MAX_HISTORY_ENTRIES, ViewPosition};

    fn position(page: u32) -> ViewPosition {
        ViewPosition {
            page,
            scroll_x: 0,
            scroll_y: 0,
            center_content: false,
        }
    }

    fn pending(page: u32) -> Option<LinkDestination> {
        Some(LinkDestination {
            page,
            top_ratio: Some(0.75),
            left_ratio: Some(0.5),
        })
    }

    fn positions(history: &JumpList) -> Vec<ViewPosition> {
        history.entries.iter().map(|entry| entry.position).collect()
    }

    #[test]
    fn back_and_forward_restore_page_scroll_and_alignment() {
        let origin = ViewPosition {
            scroll_x: -35,
            scroll_y: 42,
            ..position(1)
        };
        let destination = ViewPosition {
            scroll_x: 71,
            scroll_y: 160,
            center_content: true,
            ..position(5)
        };
        let mut history = JumpList::default();
        history.jump(origin, destination, None);
        assert_eq!(history.backward(destination), Some(origin));
        assert_eq!(history.backward(origin), None);
        assert_eq!(history.forward(), Some(destination));
        assert_eq!(history.forward(), None);
    }

    #[test]
    fn new_jump_after_back_discards_forward_branch() {
        let mut history = JumpList::default();
        history.jump(position(0), position(1), None);
        history.jump(position(1), position(2), None);
        assert_eq!(history.backward(position(2)), Some(position(1)));
        history.jump(position(1), position(3), None);
        assert_eq!(history.forward(), None);
        assert_eq!(history.backward(position(3)), Some(position(1)));
        assert_eq!(history.backward(position(1)), Some(position(0)));
        assert_eq!(history.forward(), Some(position(1)));
        assert_eq!(history.forward(), Some(position(3)));
    }

    #[test]
    fn back_preserves_current_position_after_scrolling_past_tip() {
        let mut history = JumpList::default();
        history.jump(position(0), position(2), None);
        let scrolled = ViewPosition {
            scroll_x: 45,
            scroll_y: 96,
            ..position(3)
        };
        assert_eq!(history.backward(scrolled), Some(position(2)));
        assert_eq!(history.forward(), Some(scrolled));
        assert_eq!(history.backward(scrolled), Some(position(2)));
        assert_eq!(history.backward(position(2)), Some(position(0)));
    }

    #[test]
    fn same_page_destination_records_resolved_scroll_without_losing_origin() {
        let origin = position(0);
        let destination = ViewPosition {
            scroll_x: 120,
            scroll_y: 240,
            ..origin
        };
        let mut history = JumpList::default();
        history.jump(origin, origin, pending(0));
        assert_eq!(history.backward(origin), Some(origin));
        assert_eq!(history.forward(), Some(origin));
        assert_eq!(history.pending_destination(), pending(0));
        history.update_tip(destination);
        assert_eq!(history.backward(destination), Some(origin));
        assert_eq!(history.forward(), Some(destination));
        assert_eq!(history.pending_destination(), None);
    }

    #[test]
    fn duplicate_and_noop_destinations_do_not_add_history_entries() {
        let mut history = JumpList::default();
        history.jump(position(0), position(1), None);
        history.jump(position(1), position(1), pending(1));
        history.update_tip(position(1));
        assert_eq!(positions(&history), vec![position(0), position(1)]);
        history.jump(position(1), position(2), pending(2));
        history.update_tip(position(1));
        assert_eq!(positions(&history), vec![position(0), position(1)]);
        assert_eq!(history.backward(position(1)), Some(position(0)));
    }

    #[test]
    fn deferred_destination_does_not_overwrite_a_navigated_history_entry() {
        let mut history = JumpList::default();
        history.jump(position(0), position(1), pending(1));
        assert_eq!(history.backward(position(1)), Some(position(0)));
        history.update_tip(position(4));
        assert_eq!(history.forward(), Some(position(1)));
        assert_eq!(history.pending_destination(), pending(1));
    }

    #[test]
    fn history_and_scrolled_tip_stay_bounded() {
        let mut history = JumpList::default();
        for page in 0..150 {
            history.push(position(page));
        }
        assert_eq!(history.entries.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(history.backward(position(150)), Some(position(149)));
        assert_eq!(history.entries.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(history.forward(), Some(position(150)));
        for page in (51..150).rev() {
            assert_eq!(history.backward(position(page + 1)), Some(position(page)));
        }
        assert_eq!(history.backward(position(51)), None);
    }

    #[test]
    fn reload_shrink_deduplicates_and_keeps_cursor_on_current_entry() {
        let mut history = JumpList::default();
        for page in [0, 4, 5, 6, 1] {
            history.push(position(page));
        }
        assert_eq!(history.backward(position(1)), Some(position(6)));
        history.clamp_pages(3);
        assert_eq!(
            positions(&history),
            vec![position(0), position(2), position(1)]
        );
        assert_eq!(history.backward(position(2)), Some(position(0)));
        assert_eq!(history.forward(), Some(position(2)));
        assert_eq!(history.forward(), Some(position(1)));
    }

    #[test]
    fn reload_preserves_scroll_distinctions_and_forward_entries() {
        let mut history = JumpList::default();
        let first = ViewPosition {
            scroll_x: -20,
            scroll_y: 70,
            ..position(5)
        };
        let second = ViewPosition {
            scroll_x: 30,
            scroll_y: 140,
            ..position(6)
        };
        history.jump(first, second, None);
        assert_eq!(history.backward(second), Some(first));
        history.clamp_pages(2);
        assert_eq!(history.forward(), Some(ViewPosition { page: 1, ..second }));
        assert_eq!(
            history.backward(ViewPosition { page: 1, ..second }),
            Some(ViewPosition { page: 1, ..first })
        );
    }

    #[test]
    fn reload_handles_empty_and_fully_collapsed_history() {
        let mut empty = JumpList::default();
        empty.clamp_pages(0);
        assert_eq!(empty.backward(position(0)), None);
        assert_eq!(empty.forward(), None);
        let mut history = JumpList::default();
        history.jump(position(4), position(5), None);
        history.clamp_pages(0);
        assert_eq!(positions(&history), vec![position(0)]);
        assert_eq!(history.backward(position(0)), None);
        assert_eq!(history.forward(), None);
        history.jump(position(0), position(1), None);
        assert_eq!(history.backward(position(1)), Some(position(0)));
    }
}
