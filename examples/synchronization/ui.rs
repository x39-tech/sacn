//! The ratatui terminal rendering and keyboard handling. Renders the two
//! receiver panels side-by-side. See the doc comment in `main.rs` for the
//! overall picture.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::symbols::border;
use ratatui::widgets::Block;
use sacn::OnSyncLoss;

use crate::{Panel, UNIVERSES};

/// The width of the bar, in terminal columns.
const BAR_WIDTH: usize = 3;
/// The largest skew spread (universe 1 to universe N) the `+` key dials in, in
/// milliseconds.
const MAX_SKEW_MS: u32 = 120;

const BAND_COLORS: [Color; UNIVERSES as usize] = [
    Color::Rgb(0xff, 0x5c, 0x5c), // Red
    Color::Rgb(0xff, 0xb3, 0x4d), // Orange
    Color::Rgb(0xff, 0xf0, 0x4d), // Yellow
    Color::Rgb(0x6b, 0xff, 0x6b), // Green
    Color::Rgb(0x4d, 0xc3, 0xff), // Blue
    Color::Rgb(0xc9, 0x8c, 0xff), // Purple
];

/// Runs the terminal UI: render the two panels and handle key input.
pub fn run_ui(
    off_panel: Panel,
    on_panel: Panel,
    cut: Arc<AtomicBool>,
    skew_ms: Arc<AtomicU32>,
    on_loss: OnSyncLoss,
) -> anyhow::Result<()> {
    let (key_tx, key_rx) = std::sync::mpsc::channel::<KeyEvent>();
    std::thread::spawn(move || loop {
        if let Ok(TermEvent::Key(key)) = event::read() {
            if key_tx.send(key).is_err() {
                break;
            }
        }
    });

    let mut terminal = ratatui::init();
    loop {
        let off = *off_panel.lock().unwrap();
        let on = *on_panel.lock().unwrap();
        let is_cut = cut.load(Ordering::Relaxed);
        let skew = skew_ms.load(Ordering::Relaxed);
        terminal.draw(|frame| render(frame, &off, &on, is_cut, skew, on_loss))?;

        // Poll for input at the redraw cadence.
        if let Ok(key) = key_rx.recv_timeout(Duration::from_millis(33)) {
            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Char(' ') => {
                    cut.fetch_xor(true, Ordering::Relaxed);
                }
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    let _ = skew_ms.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |s| {
                        Some((s + 2).min(MAX_SKEW_MS))
                    });
                }
                KeyCode::Char('-') | KeyCode::Char('_') => {
                    let _ = skew_ms.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |s| {
                        Some(s.saturating_sub(2))
                    });
                }
                _ => {}
            }
        }
    }
    ratatui::restore();
    Ok(())
}

/// Renders the whole screen: a title, the two panels, and the status footer.
fn render(
    frame: &mut Frame,
    off: &[Option<u8>; UNIVERSES as usize],
    on: &[Option<u8>; UNIVERSES as usize],
    is_cut: bool,
    skew: u32,
    on_loss: OnSyncLoss,
) {
    let [title, main, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    frame.render_widget(
        Line::from("E1.31 Universe Synchronization".bold()).centered(),
        title,
    );

    let [left, right] =
        Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).areas(main);
    render_panel(frame, left, "Sync OFF", off);
    render_panel(frame, right, "Sync ON", on);

    let policy = match on_loss {
        OnSyncLoss::HoldLastLook => "hold last look",
        OnSyncLoss::RevertToLive => "revert to live",
    };
    let sync_state = if is_cut {
        "CUT".to_string().red().bold()
    } else {
        "LIVE".to_string().green().bold()
    };
    let status = Line::from(vec![
        "skew ".into(),
        format!("{skew}ms").cyan(),
        "   sync stream ".into(),
        sync_state,
        format!("   on-loss: {policy}").into(),
    ])
    .centered();
    let help = Line::from(
        "q quit   space cut/restore sync   +/- skew"
            .to_string()
            .dark_gray(),
    )
    .centered();
    let [status_rect, help_rect] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(footer);
    frame.render_widget(status, status_rect);
    frame.render_widget(help, help_rect);
}

/// Renders one receiver panel: a bordered stack of bands that together tile the
/// full inner height, each band a solid block of rows showing that universe's
/// bar at the column its latest frame set.
fn render_panel(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    bands: &[Option<u8>; UNIVERSES as usize],
) {
    let block = Block::bordered()
        .border_set(border::THICK)
        .title(Line::from(title).centered());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 2 || inner.height < UNIVERSES {
        return;
    }
    let width = inner.width as usize;
    // Split the height into UNIVERSES contiguous slices that sum exactly to
    // inner.height, spreading the remainder across the first bands so the stack
    // fills the panel from top to bottom.
    let base = inner.height / UNIVERSES;
    let remainder = inner.height % UNIVERSES;

    for (i, value) in bands.iter().enumerate() {
        let i = i as u16;
        let height = base + u16::from(i < remainder);
        let y = inner.y + i * base + i.min(remainder);
        let line = horizontal_row(*value, width, BAND_COLORS[i as usize]);
        for row_y in y..y + height {
            let row = Rect {
                x: inner.x,
                y: row_y,
                width: inner.width,
                height: 1,
            };
            frame.render_widget(line.clone(), row);
        }
    }
}

/// Builds a horizontal row: a full-width line of dim dots (the background)
/// with a [`BAR_WIDTH`]-wide run of bright blocks (the bar) drawn over it.
/// The `value`, `0..=255`, is scaled to a column across the row's `width`
/// and the bar is centered there. A `None` value means no data has arrived
/// for this universe yet, so the whole row is left as dots with no bar.
fn horizontal_row(value: Option<u8>, width: usize, color: Color) -> Line<'static> {
    let track = Style::default().fg(Color::Rgb(0x30, 0x30, 0x30));
    let Some(value) = value else {
        return Line::from(Span::styled("·".repeat(width), track));
    };
    // Center of the bar, then its half-open column span clamped to the panel.
    let center = value as usize * width.saturating_sub(1) / 255;
    let start = center.saturating_sub(BAR_WIDTH / 2);
    let end = (start + BAR_WIDTH).min(width);
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span::styled("·".repeat(start), track));
    }
    spans.push(Span::styled(
        "█".repeat(end - start),
        Style::default().fg(color),
    ));
    if end < width {
        spans.push(Span::styled("·".repeat(width - end), track));
    }
    Line::from(spans)
}
