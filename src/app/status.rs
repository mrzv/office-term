use std::cell::Cell;
use std::fmt;
use std::io::{self, Write};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::Span;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PerformanceSnapshot {
    pub(super) render_ms: u128,
    pub(super) dark_mode_ms: Option<u128>,
    pub(super) highlight_ms: Option<u128>,
    pub(super) compression_ms: u128,
    pub(super) transfer_ms: u128,
    pub(super) link_count: usize,
}

impl PerformanceSnapshot {
    pub(super) fn status(self, detailed: bool, link_mode: bool) -> String {
        let mut status = render_timing_status(
            self.render_ms,
            self.dark_mode_ms,
            self.highlight_ms,
            self.compression_ms,
            self.transfer_ms,
            detailed,
        );
        if link_mode {
            status.push_str(&format!("  {} page links", self.link_count));
        }
        status
    }
}

pub(super) fn render_timing_status(
    render_ms: u128,
    dark_mode_ms: Option<u128>,
    highlight_ms: Option<u128>,
    compression_ms: u128,
    transfer_ms: u128,
    detailed: bool,
) -> String {
    if detailed {
        let dark_mode = dark_mode_ms
            .map(|elapsed| format!("  dark {elapsed}ms"))
            .unwrap_or_default();
        let highlight = highlight_ms
            .map(|elapsed| format!("  highlight {elapsed}ms"))
            .unwrap_or_default();
        return format!(
            "render {render_ms}ms{dark_mode}{highlight}  compress {compression_ms}ms  transfer {transfer_ms}ms"
        );
    }

    let total_ms = render_ms
        + dark_mode_ms.unwrap_or_default()
        + highlight_ms.unwrap_or_default()
        + compression_ms
        + transfer_ms;
    format!("render {total_ms}ms")
}

/// Writes text to the system clipboard using the OSC 52 escape sequence, which
/// works over SSH because the terminal emulator performs the copy locally.
pub(super) fn write_clipboard_osc52(output: &mut impl Write, text: &str) -> io::Result<()> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    write!(output, "\x1b]52;c;{encoded}\x07")?;
    output.flush()
}

/// Clip all styled status fragments to one row without emitting text controls.
pub(super) struct StatusText<'a, T>(pub(super) T, pub(super) &'a Cell<usize>);

impl<T: fmt::Display> fmt::Display for StatusText<'_, T> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write as _;
        write!(
            StatusWriter {
                output,
                columns: self.1
            },
            "{}",
            self.0
        )
    }
}

struct StatusWriter<'a, 'b> {
    output: &'a mut fmt::Formatter<'b>,
    columns: &'a Cell<usize>,
}

impl fmt::Write for StatusWriter<'_, '_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        for part in value.split_inclusive(char::is_control) {
            let span = Span::raw(part);
            for grapheme in span.styled_graphemes(Style::default()) {
                let width = Span::raw(grapheme.symbol).width();
                if self.columns.get() == 0 || width > self.columns.get() {
                    self.columns.set(0);
                    return Ok(());
                }
                self.output.write_str(grapheme.symbol)?;
                self.columns.set(self.columns.get() - width);
            }
            if part.ends_with(char::is_control) && self.columns.get() > 0 {
                self.output.write_str(" ")?;
                self.columns.set(self.columns.get() - 1);
            }
        }
        Ok(())
    }
}

pub(super) fn stale_status_row(previous: Option<u16>, current: u16) -> Option<u16> {
    previous.filter(|row| *row < current)
}

pub(super) fn cycled_tab_index(active: usize, len: usize, direction: i32) -> usize {
    if direction > 0 {
        (active + 1) % len
    } else if active == 0 {
        len - 1
    } else {
        active - 1
    }
}

pub(super) fn numbered_tab_index(key: KeyEvent) -> Option<usize> {
    let digit = match key.code {
        KeyCode::Char(digit) if ('1'..='9').contains(&digit) => digit,
        _ => return None,
    };
    key.modifiers
        .contains(KeyModifiers::ALT)
        .then(|| usize::from(digit as u8 - b'1'))
}
