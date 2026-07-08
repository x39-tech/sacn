//! A terminal sACN source console.
//!
//! Transmits on any number of universes at once and lets you drive each one
//! live: add and remove universes, pick an animated pattern, change priority,
//! toggle the preview flag, and adjust the frame rate, all from the keyboard.
//! Run it with `cargo run --example source_console` and watch it with the
//! `receiver` or `merge_receiver` example. Try running multiple `source_console`
//! examples on the same universe and watching their merged output with the
//! `merge_receiver` example.

#[path = "common/tui.rs"]
mod common;

use std::{thread, time::Duration};

use common::{
    init_tui_logging, is_quit_event, render_chrome, render_level_grid, valid_universe, Event,
    PickerOutcome, UniversePicker, SLOTS,
};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use layout::Flex;
use ratatui::{
    prelude::*,
    symbols::border,
    widgets::{Block, Clear, Paragraph},
};
use sacn::tokio::Source;
use sacn::{Cid, Priority, SourceConfig, Universe, UniverseConfig};
use tracing::{error, info, warn};
use uuid::Uuid;

/// The slowest and fastest frame rates the rate control allows, in Hz.
const MIN_RATE: u32 = 1;
const MAX_RATE: u32 = 60;
const INITIAL_RATE: u32 = 30;
const INITIAL_UNIVERSE: u16 = 1;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let log_buf = init_tui_logging(event_tx.clone());

    let cid: Cid = Uuid::new_v4().into();
    let source = Source::bind(SourceConfig::new(cid, "x39 sACN console")).await?;

    let mut app = App::new(source, log_buf);
    app.add_universe(INITIAL_UNIVERSE);

    let event_thread = thread::spawn(move || -> anyhow::Result<()> {
        let mut run = true;
        while run {
            let e = event::read()?;
            if is_quit_event(&e) {
                run = false;
            }
            event_tx.send(Event::Term(e))?;
        }
        Ok(())
    });

    info!("sACN source console created");

    let mut terminal = ratatui::init();
    let mut frame = tokio::time::interval(app.frame_period());
    // The first tick fires immediately; we have nothing to advance yet, so skip
    // it and let the interval settle into its cadence.
    frame.tick().await;

    let result = run(&mut app, &mut terminal, &mut event_rx, &mut frame).await;

    let _ = terminal.draw(|f| app.render_terminating(f));
    app.shutdown().await;
    event_thread
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e))?;
    ratatui::restore();

    result
}

/// The main event loop.
async fn run(
    app: &mut App,
    terminal: &mut ratatui::DefaultTerminal,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    frame: &mut tokio::time::Interval,
) -> anyhow::Result<()> {
    redraw(terminal, app)?;
    loop {
        let deadline = app.process().await?;
        let wait = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            event = event_rx.recv() => match event {
                Some(Event::Term(event)) => {
                    if is_quit_event(&event) {
                        break;
                    }
                    if app.handle_event(event) == RateChanged::Yes {
                        *frame = tokio::time::interval(app.frame_period());
                        frame.tick().await;
                    }
                    redraw(terminal, app)?;
                }
                Some(Event::Log) => redraw(terminal, app)?,
                None => break,
            },
            _ = frame.tick() => {
                app.tick();
                redraw(terminal, app)?;
            }
            // A transmission deadline: `process()` above sent whatever was due.
            // The displayed levels did not change, so there is nothing to redraw.
            () = wait => {}
        }
    }
    Ok(())
}

/// Renders the current application state to the terminal.
fn redraw(terminal: &mut ratatui::DefaultTerminal, app: &App) -> anyhow::Result<()> {
    terminal.draw(|f| app.render_ui(f))?;
    Ok(())
}

/// Whether a key press changed the frame rate, so the caller can rebuild the
/// frame interval.
#[derive(PartialEq, Eq)]
enum RateChanged {
    Yes,
    No,
}

/// The animated patterns a universe can transmit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pattern {
    /// A single high slot sweeping across the universe.
    Chase,
    /// A level ramp that scrolls along the universe.
    Ramp,
    /// A sine wave rippling across the universe.
    Sine,
    /// Every slot at full.
    AllFull,
    /// Every slot at 0.
    AllZero,
}

impl Pattern {
    const ALL: [Pattern; 5] = [
        Pattern::Chase,
        Pattern::Ramp,
        Pattern::Sine,
        Pattern::AllFull,
        Pattern::AllZero,
    ];

    fn next(self) -> Pattern {
        let idx = Pattern::ALL.iter().position(|&p| p == self).unwrap_or(0);
        Pattern::ALL[(idx + 1) % Pattern::ALL.len()]
    }

    fn label(self) -> &'static str {
        match self {
            Pattern::Chase => "chase",
            Pattern::Ramp => "ramp",
            Pattern::Sine => "sine",
            Pattern::AllFull => "all full",
            Pattern::AllZero => "all zero",
        }
    }

    /// Fills `levels` for animation step `phase` (the running frame count).
    fn fill(self, levels: &mut [u8; SLOTS], phase: usize) {
        match self {
            Pattern::Chase => {
                levels.fill(0);
                levels[phase % SLOTS] = 255;
            }
            Pattern::Ramp => {
                for (i, level) in levels.iter_mut().enumerate() {
                    *level = ((i + phase) % 256) as u8;
                }
            }
            Pattern::Sine => {
                for (i, level) in levels.iter_mut().enumerate() {
                    let theta = (i + phase) as f32 * 0.1;
                    *level = ((theta.sin() * 0.5 + 0.5) * 255.0) as u8;
                }
            }
            Pattern::AllFull => levels.fill(255),
            Pattern::AllZero => levels.fill(0),
        }
    }
}

/// One universe the console is transmitting on, with its editable settings and
/// the level buffer it animates.
struct UniverseSlot {
    universe: Universe,
    pattern: Pattern,
    priority: Priority,
    preview: bool,
    phase: usize,
    levels: [u8; SLOTS],
}

impl UniverseSlot {
    fn new(universe: Universe) -> Self {
        Self {
            universe,
            pattern: Pattern::Chase,
            priority: Priority::DEFAULT,
            preview: false,
            phase: 0,
            levels: [0; SLOTS],
        }
    }
}

enum AppMode {
    Normal,
    AddUniverse(UniversePicker),
}

struct App {
    source: Source,
    slots: Vec<UniverseSlot>,
    /// Index into `slots` of the universe the controls currently act on.
    selected: usize,
    rate: u32,
    mode: AppMode,
    log: std::sync::Arc<std::sync::Mutex<common::AppLogBuf>>,
}

impl App {
    fn new(source: Source, log: std::sync::Arc<std::sync::Mutex<common::AppLogBuf>>) -> Self {
        Self {
            source,
            slots: Vec::new(),
            selected: 0,
            rate: INITIAL_RATE,
            mode: AppMode::Normal,
            log,
        }
    }

    fn frame_period(&self) -> Duration {
        Duration::from_micros(1_000_000 / self.rate as u64)
    }

    /// The universe the controls currently act on, if any.
    fn current(&self) -> Option<&UniverseSlot> {
        self.slots.get(self.selected)
    }

    /// Advances every universe's animation one frame and pushes the new levels
    /// into the source.
    fn tick(&mut self) {
        for slot in &mut self.slots {
            slot.phase = slot.phase.wrapping_add(1);
            slot.pattern.fill(&mut slot.levels, slot.phase);
            self.source.update_levels(slot.universe, &slot.levels);
        }
    }

    async fn process(&mut self) -> anyhow::Result<Option<tokio::time::Instant>> {
        Ok(self.source.process().await?)
    }

    /// Registers a new universe with the source and adds a slot for it. No-op if
    /// the number is out of range or already present.
    fn add_universe(&mut self, universe: u16) {
        let Some(universe) = valid_universe(universe) else {
            return;
        };
        if self.slots.iter().any(|s| s.universe == universe) {
            warn!("already transmitting on universe {universe}");
            return;
        }
        match self.source.add_universe(UniverseConfig::new(universe)) {
            Ok(_) => {
                let mut slot = UniverseSlot::new(universe);
                slot.pattern.fill(&mut slot.levels, slot.phase);
                self.source.update_levels(universe, &slot.levels);
                self.slots.push(slot);
                self.slots.sort_by_key(|s| s.universe.get());
                self.selected = self
                    .slots
                    .iter()
                    .position(|s| s.universe == universe)
                    .unwrap_or(0);
                info!("transmitting on universe {universe}");
            }
            Err(error) => error!("could not add universe {universe}: {error}"),
        }
    }

    /// Begins terminating the selected universe and drops its slot.
    fn remove_selected(&mut self) {
        let Some(slot) = self.slots.get(self.selected) else {
            return;
        };
        let universe = slot.universe;
        self.source.remove_universe(universe);
        self.slots.remove(self.selected);
        self.selected = self.selected.min(self.slots.len().saturating_sub(1));
        info!("terminating universe {universe}");
    }

    fn handle_event(&mut self, event: TermEvent) -> RateChanged {
        let TermEvent::Key(KeyEvent {
            code, modifiers, ..
        }) = event
        else {
            return RateChanged::No;
        };

        match &mut self.mode {
            AppMode::AddUniverse(picker) => {
                match picker.handle_key(code) {
                    PickerOutcome::Confirmed(universe) => {
                        self.mode = AppMode::Normal;
                        if let Some(universe) = universe {
                            self.add_universe(universe);
                        }
                    }
                    PickerOutcome::Cancelled => self.mode = AppMode::Normal,
                    PickerOutcome::Pending => {}
                }
                RateChanged::No
            }
            AppMode::Normal => self.handle_normal_key(code, modifiers),
        }
    }

    fn handle_normal_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> RateChanged {
        match (code, modifiers) {
            (KeyCode::Char('a'), KeyModifiers::NONE) => {
                self.mode = AppMode::AddUniverse(UniversePicker::new(None));
            }
            (KeyCode::Char('x'), KeyModifiers::NONE) => self.remove_selected(),
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.selected = self.selected.saturating_sub(1);
            }
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
                if !self.slots.is_empty() {
                    self.selected = (self.selected + 1).min(self.slots.len() - 1);
                }
            }
            (KeyCode::Char('p'), KeyModifiers::NONE) => {
                if let Some(slot) = self.slots.get_mut(self.selected) {
                    slot.pattern = slot.pattern.next();
                    slot.pattern.fill(&mut slot.levels, slot.phase);
                    self.source.update_levels(slot.universe, &slot.levels);
                }
            }
            (KeyCode::Char('v'), KeyModifiers::NONE) => {
                if let Some(slot) = self.slots.get_mut(self.selected) {
                    slot.preview = !slot.preview;
                    self.source.set_preview(slot.universe, slot.preview);
                }
            }
            (KeyCode::Char(']'), KeyModifiers::NONE) => self.change_priority(1),
            (KeyCode::Char('['), KeyModifiers::NONE) => self.change_priority(-1),
            (KeyCode::Char('+' | '='), _) => return self.set_rate(self.rate + 1),
            (KeyCode::Char('-'), _) => return self.set_rate(self.rate.saturating_sub(1)),
            _ => {}
        }
        RateChanged::No
    }

    /// Adjusts the selected universe's priority by `delta`, clamped to the legal
    /// range, and pushes it to the source.
    fn change_priority(&mut self, delta: i16) {
        let Some(slot) = self.slots.get_mut(self.selected) else {
            return;
        };
        let next = (slot.priority.get() as i16 + delta).clamp(0, Priority::MAX as i16) as u8;
        // `next` is clamped into range, so this cannot fail.
        if let Ok(priority) = Priority::new(next) {
            slot.priority = priority;
            self.source.set_priority(slot.universe, priority);
        }
    }

    fn set_rate(&mut self, rate: u32) -> RateChanged {
        let rate = rate.clamp(MIN_RATE, MAX_RATE);
        if rate == self.rate {
            return RateChanged::No;
        }
        self.rate = rate;
        RateChanged::Yes
    }

    /// Terminates every universe cleanly, draining the stream-terminated packets.
    async fn shutdown(&mut self) {
        for slot in &self.slots {
            self.source.remove_universe(slot.universe);
        }
        while self.source.universes().next().is_some() {
            match self.source.process().await {
                Ok(Some(at)) => tokio::time::sleep_until(at).await,
                Ok(None) => break,
                Err(_) => break,
            }
        }
        info!("all streams terminated");
    }

    fn render_ui(&self, frame: &mut Frame) {
        let instructions = Line::from(vec![
            " Add ".into(),
            "<A>".blue().bold(),
            " Remove ".into(),
            "<X>".blue().bold(),
            " Select ".into(),
            "<Up/Down>".blue().bold(),
            " Pattern ".into(),
            "<P>".blue().bold(),
            " Priority ".into(),
            "<[ ]>".blue().bold(),
            " Preview ".into(),
            "<V>".blue().bold(),
            " Rate ".into(),
            "<+ ->".blue().bold(),
            " Quit ".into(),
            "<Q> ".blue().bold(),
        ]);

        let (data_rect, info_rect) = render_chrome(
            frame,
            "sACN Source Console Example",
            instructions,
            &self.log,
        );

        let title = match self.current() {
            Some(slot) => format!("Transmitting - Universe {}", slot.universe),
            None => "Transmitting - no universes".to_string(),
        };
        render_level_grid(frame, data_rect, &title, self.current().map(|s| &s.levels));
        self.render_info(frame, info_rect);

        if let AppMode::AddUniverse(picker) = &self.mode {
            picker.render(frame, "Universe to add");
        }
    }

    /// Renders the normal view with a centered "terminating" notice over it,
    /// shown while the stream-terminated sequence drains on shutdown.
    fn render_terminating(&self, frame: &mut Frame) {
        self.render_ui(frame);

        let [area] = Layout::horizontal([Constraint::Length(24)])
            .flex(Flex::Center)
            .areas(frame.area());
        let [area] = Layout::vertical([Constraint::Length(3)])
            .flex(Flex::Center)
            .areas(area);

        let notice = Paragraph::new(Line::from("Terminating sACN...".yellow().bold()).centered())
            .block(Block::bordered().border_set(border::THICK));
        frame.render_widget(Clear, area);
        frame.render_widget(notice, area);
    }

    fn render_info(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![
            Line::from(format!("Rate: {} Hz", self.rate)),
            Line::from(""),
            Line::from(format!("Universes ({})", self.slots.len()).bold()),
        ];
        if self.slots.is_empty() {
            lines.push(Line::from("none - press <A> to add".dim()));
        } else {
            for (i, slot) in self.slots.iter().enumerate() {
                let marker = if i == self.selected { ">" } else { " " };
                let preview = if slot.preview { " preview" } else { "" };
                let line = format!(
                    "{marker} U{} {} p{}{}",
                    slot.universe,
                    slot.pattern.label(),
                    slot.priority.get(),
                    preview,
                );
                let line = if i == self.selected {
                    Line::from(line.bold().green())
                } else {
                    Line::from(line)
                };
                lines.push(line);
            }
        }

        let info = Paragraph::new(lines).block(Block::bordered().border_set(border::THICK));
        frame.render_widget(info, area);
    }
}
