//! Shared TUI scaffolding for the sACN examples.
//!
//! The receiver and source example binaries all need the same handful of pieces:
//! a way to pump terminal events and tracing output through one `select!` loop,
//! a popup for entering a universe number, a grid that renders 512 DMX slots, and
//! the title/log/instructions chrome around it. They live here so each example
//! can focus on the part that is actually interesting.
//!
//! This is a module shared between examples, so not every example uses every
//! item; `dead_code` is allowed accordingly.
#![allow(dead_code)]

use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
};

use ansi_to_tui::IntoText;
use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use layout::Flex;
use ratatui::{
    prelude::*,
    symbols::border,
    widgets::{Block, Clear, Paragraph},
};
use sacn::Universe;
use tokio::sync::mpsc::UnboundedSender;
use tracing::Level;

/// The number of log lines kept on screen.
pub const MAX_LOG_LINES: u16 = 10;

/// The number of DMX slots in a full universe.
pub const SLOTS: usize = 512;

/// An event delivered to an example's main loop: either a terminal event or a
/// nudge that the in-memory log has new content to redraw.
pub enum Event {
    Term(TermEvent),
    Log,
}

/// Whether `event` is the quit chord (`q` or Ctrl-C).
pub fn is_quit_event(event: &TermEvent) -> bool {
    matches!(
        *event,
        TermEvent::Key(
            KeyEvent {
                code: KeyCode::Char('q'),
                ..
            } | KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        )
    )
}

/// Converts a raw universe number into a validated [`Universe`], or `None` if it
/// is out of range.
pub fn valid_universe(universe: u16) -> Option<Universe> {
    Universe::new(universe).ok()
}

/// A short, human-readable stand-in for a source with no usable name, derived
/// from the leading bytes of its CID.
pub fn display_cid(cid: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}...",
        cid[0], cid[1], cid[2], cid[3]
    )
}

/// A short, human-friendly label for a source: its name if it sent one,
/// otherwise the leading bytes of its CID.
pub fn display_source(cid: &[u8; 16], name: &str) -> String {
    if name.is_empty() {
        display_cid(cid)
    } else {
        name.to_string()
    }
}

/// Renders the standard example chrome: a centered title, the scrolling log
/// pane, and the instructions footer. Returns the `(data, info)` rects in the
/// middle for the example to fill with its own content.
pub fn render_chrome(
    frame: &mut Frame,
    title: &str,
    instructions: Line,
    log: &Arc<Mutex<AppLogBuf>>,
) -> (Rect, Rect) {
    let [title_rect, main_rect, log_rect, instructions_rect] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(MAX_LOG_LINES + 2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let [data_rect, info_rect] =
        Layout::horizontal([Constraint::Ratio(3, 4), Constraint::Ratio(1, 4)]).areas(main_rect);

    let log_lines: Vec<Line> = log.lock().unwrap().0.iter().cloned().collect();
    let log_block = Block::bordered()
        .border_set(border::THICK)
        .title(Line::from(" Log ").centered());
    let log_para = Paragraph::new(Text::from(log_lines)).block(log_block);

    frame.render_widget(Line::from(title).centered(), title_rect);
    frame.render_widget(log_para, log_rect);
    frame.render_widget(instructions.centered(), instructions_rect);

    (data_rect, info_rect)
}

/// Renders a bordered grid of all 512 DMX slot values into `area` under `title`.
///
/// Pass `Some(levels)` to show live data, or `None` to draw just the row/column
/// headers while waiting for the first frame.
pub fn render_level_grid(frame: &mut Frame, area: Rect, title: &str, levels: Option<&[u8; SLOTS]>) {
    let block = Block::bordered()
        .border_set(border::THICK)
        .title(Line::from(title).centered());
    let block_inner = block.inner(area);

    frame.render_widget(block, area);

    // Manual layout for the level grid. The inner area can be divided into a
    // certain number of 4-column sections (4 columns allowing for a 3-digit
    // decimal number, 0-255, plus a space between). We limit this to even
    // numbers (to allow easier addition) and leave room for headers on the left.
    //
    // Here is an example with a width that could accommodate 12 columns:
    // 8 spaces of left padding
    //~~~~~~~~
    //        1   2   3   4   5   6   7   8   9   10  11  12
    // + 12   0   255 0   255 0   255 0   255 0   255 0   255
    // + 24   255 0   255 0   255 0   255 0   255 0   255 0

    const LEFT_PADDING: u16 = 8;

    if block_inner.width < LEFT_PADDING + 8 || block_inner.height < 2 {
        // We cannot show any data in a block this small.
        return;
    }

    let mut max_columns = (block_inner.width - LEFT_PADDING) / 4;
    if !max_columns.is_multiple_of(2) {
        max_columns -= 1;
    }
    let max_rows = block_inner.height - 1;
    assert!(max_columns >= 2);
    assert!(max_rows >= 1);

    // Render the top header
    for i in 0..max_columns {
        let line = Line::from((i + 1).to_string().bold()).left_aligned();
        let area = Rect {
            x: block_inner.x + LEFT_PADDING + (i * 4),
            y: block_inner.y,
            width: 4,
            height: 1,
        };
        frame.render_widget(line, area);
    }

    for row in 0..max_rows {
        let min_slot_represented = row * max_columns;
        if min_slot_represented as usize >= SLOTS {
            break;
        }

        let row_header =
            Line::from(format!(" + {addend}", addend = max_columns * row).bold()).left_aligned();
        let area = Rect {
            x: block_inner.x,
            y: block_inner.y + row + 1,
            width: LEFT_PADDING,
            height: 1,
        };
        frame.render_widget(row_header, area);

        let Some(levels) = levels else {
            continue;
        };

        for col in 0..max_columns {
            let slot = ((row * max_columns) + col) as usize;
            if slot >= SLOTS {
                break;
            }

            let line_val = Line::from(levels[slot].to_string()).left_aligned();
            let area = Rect {
                x: block_inner.x + LEFT_PADDING + (col * 4),
                y: block_inner.y + row + 1,
                width: 4,
                height: 1,
            };
            frame.render_widget(line_val, area);
        }
    }
}

/// The outcome of feeding a key to a [`UniversePicker`].
pub enum PickerOutcome {
    /// The picker is still open and editing.
    Pending,
    /// The user confirmed; the entered universe (if any) is returned.
    Confirmed(Option<u16>),
    /// The user cancelled.
    Cancelled,
}

/// A small modal for entering a universe number, validating each edit against
/// the legal sACN range so the value held is always in range (or empty).
pub struct UniversePicker {
    value: Option<u16>,
}

impl UniversePicker {
    /// Opens a picker pre-filled with `initial` (typically the current universe).
    pub fn new(initial: Option<u16>) -> Self {
        Self { value: initial }
    }

    /// The currently entered universe, if any.
    pub fn value(&self) -> Option<u16> {
        self.value
    }

    /// Feeds a key press to the picker, returning what should happen next.
    pub fn handle_key(&mut self, code: KeyCode) -> PickerOutcome {
        match code {
            KeyCode::Enter => PickerOutcome::Confirmed(self.value),
            KeyCode::Esc => PickerOutcome::Cancelled,
            KeyCode::Char(ch @ '0'..='9') => {
                let mut entry = match self.value {
                    Some(u) => u.to_string(),
                    None => String::new(),
                };
                entry.push(ch);
                if let Ok(new) = entry.parse() {
                    if valid_universe(new).is_some() {
                        self.value = Some(new);
                    }
                }
                PickerOutcome::Pending
            }
            KeyCode::Backspace => {
                if let Some(universe) = self.value {
                    let mut entry = universe.to_string();
                    entry.pop();
                    self.value = entry.parse().ok().filter(|&u| valid_universe(u).is_some());
                }
                PickerOutcome::Pending
            }
            _ => PickerOutcome::Pending,
        }
    }

    /// Renders the picker as a centered popup with the given prompt.
    pub fn render(&self, frame: &mut Frame, prompt: &str) {
        let [area] = Layout::horizontal([Constraint::Length(40)])
            .flex(Flex::Center)
            .areas(frame.area());
        let [area] = Layout::vertical([Constraint::Length(3)])
            .flex(Flex::Center)
            .areas(area);

        let popup_block = Block::bordered()
            .border_set(border::THICK)
            .title(Line::from(prompt).centered());

        let mut text = match self.value {
            Some(universe) => universe.to_string(),
            None => String::new(),
        };
        text.insert(0, ' ');
        let paragraph = Paragraph::new(text).block(popup_block);
        frame.render_widget(Clear, area);
        frame.render_widget(paragraph, area);
    }
}

/// An in-memory ring buffer of the most recent rendered log lines.
pub struct AppLogBuf(pub VecDeque<Line<'static>>);

impl AppLogBuf {
    fn new() -> Self {
        Self(VecDeque::new())
    }

    fn log_bytes(&mut self, buf: &[u8]) {
        let text = buf.into_text().unwrap_or(Text::from("<invalid utf8>"));
        let log = &mut self.0;
        for line in text.lines {
            log.push_back(line);
        }
        while log.len() > MAX_LOG_LINES as usize {
            log.pop_front();
        }
    }
}

/// A `tracing` writer that appends formatted, ANSI-styled log output into a
/// shared [`AppLogBuf`] and nudges the main loop to redraw.
#[derive(Clone)]
pub struct AppLogWriter {
    pub log: Arc<Mutex<AppLogBuf>>,
    event_tx: UnboundedSender<Event>,
}

impl AppLogWriter {
    fn new(event_tx: UnboundedSender<Event>) -> Self {
        Self {
            log: Arc::new(Mutex::new(AppLogBuf::new())),
            event_tx,
        }
    }
}

impl io::Write for AppLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.log.lock().unwrap().log_bytes(buf);
        let _ = self.event_tx.send(Event::Log);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Installs a tracing subscriber that renders into an in-memory log buffer (so
/// log output does not corrupt the TUI), returning the buffer to draw from.
pub fn init_tui_logging(event_tx: UnboundedSender<Event>) -> Arc<Mutex<AppLogBuf>> {
    let writer = AppLogWriter::new(event_tx);
    let log_buf = writer.log.clone();
    tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(Level::TRACE)
        .init();
    log_buf
}
