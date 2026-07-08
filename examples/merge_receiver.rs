//! A terminal sACN merging receiver.
//!
//! Listens on a universe and renders its **merged** DMX levels live in a TUI,
//! alongside the list of active sources contributing to the merge; press `u` to
//! switch universes. Run it with `cargo run --example merge_receiver`.

#[path = "common/tui.rs"]
mod common;

use std::{
    sync::{Arc, Mutex},
    thread,
};

use common::{
    display_source, init_tui_logging, is_quit_event, render_chrome, render_level_grid,
    valid_universe, AppLogBuf, Event, PickerOutcome, UniversePicker, SLOTS,
};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent};
use ratatui::{
    prelude::*,
    symbols::border,
    widgets::{Block, Paragraph},
};
use sacn::tokio::Receiver;
use sacn::{ReceiverConfig, ReceiverEvent};
use tracing::{error, info, warn};

const INITIAL_UNIVERSE: u16 = 1;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let receiver = Receiver::bind(ReceiverConfig::new()).await?;

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let log_buf = init_tui_logging(event_tx.clone());

    let mut app = App::new(receiver, INITIAL_UNIVERSE, log_buf).await;

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

    info!("sACN merge receiver created");

    let mut terminal = ratatui::init();

    loop {
        terminal.draw(|frame| app.render_ui(frame))?;
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(Event::Term(event)) => if is_quit_event(&event) {
                        break;
                    } else {
                        app.handle_event(event).await;
                    }
                    Some(Event::Log) => {},
                    None => { break }
                }
            },
            _ = app.process_sacn() => {}
        }
    }

    event_thread
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e))?;
    ratatui::restore();

    Ok(())
}

enum AppMode {
    Normal,
    UniversePicker(UniversePicker),
}

struct App {
    log: Arc<Mutex<AppLogBuf>>,
    receiver: Receiver,
    universe: u16,
    merged_levels: [u8; SLOTS],
    merged_valid: bool,
    universe_active: bool,
    sampling: bool,
    /// Names of the sources currently contributing to the merge.
    active_sources: Vec<String>,
    mode: AppMode,
}

impl App {
    async fn new(
        mut receiver: Receiver,
        initial_universe: u16,
        log: Arc<Mutex<AppLogBuf>>,
    ) -> Self {
        if let Some(universe) = valid_universe(initial_universe) {
            if let Err(e) = receiver.listen(universe).await {
                error!("Listening on universe {initial_universe} failed: {e}");
            }
        }

        Self {
            log,
            receiver,
            universe: initial_universe,
            merged_levels: [0; SLOTS],
            merged_valid: false,
            universe_active: false,
            sampling: false,
            active_sources: Vec::new(),
            mode: AppMode::Normal,
        }
    }

    async fn process_sacn(&mut self) {
        let Some(event) = self.receiver.next_event().await else {
            return;
        };
        match event {
            ReceiverEvent::MergedData(data) if data.universe.get() == self.universe => {
                self.merged_levels.copy_from_slice(data.levels());
                self.merged_valid = true;
                self.universe_active = true;
                self.active_sources = data
                    .active_sources()
                    .map(|source| display_source(source.cid.as_bytes(), &source.name))
                    .collect();
            }
            ReceiverEvent::SamplingStarted { universe } if universe.get() == self.universe => {
                self.sampling = true;
            }
            ReceiverEvent::SamplingEnded { universe } if universe.get() == self.universe => {
                self.sampling = false;
            }
            ReceiverEvent::SourcesLost { universe, sources } if universe.get() == self.universe => {
                for source in sources {
                    warn!("Source {} lost on universe {universe}", source.name);
                }
                if self.active_sources.is_empty() {
                    self.universe_active = false;
                }
            }
            _ => {}
        }
    }

    async fn handle_event(&mut self, event: TermEvent) {
        // Keyboard interaction only for now
        let TermEvent::Key(KeyEvent { code, .. }) = event else {
            return;
        };

        match &mut self.mode {
            AppMode::Normal => {
                if code == KeyCode::Char('u') {
                    self.mode = AppMode::UniversePicker(UniversePicker::new(Some(self.universe)));
                }
            }
            AppMode::UniversePicker(picker) => match picker.handle_key(code) {
                PickerOutcome::Confirmed(universe) => {
                    self.mode = AppMode::Normal;
                    if let Some(universe) = universe {
                        if universe != self.universe {
                            self.switch_universe(universe).await;
                        }
                    }
                }
                PickerOutcome::Cancelled => self.mode = AppMode::Normal,
                PickerOutcome::Pending => {}
            },
        }
    }

    async fn switch_universe(&mut self, universe: u16) {
        let Some(new_universe) = valid_universe(universe) else {
            return;
        };
        if let Some(old_universe) = valid_universe(self.universe) {
            if let Err(e) = self.receiver.stop_listening(old_universe).await {
                warn!(
                    "Error stopping listening on universe {}: {e}",
                    self.universe
                );
            }
        }

        match self.receiver.listen(new_universe).await {
            Ok(()) => {
                info!("Now listening on universe {universe}");
                self.merged_valid = false;
                self.universe_active = false;
                self.sampling = false;
                self.active_sources.clear();
                self.universe = universe;
            }
            Err(e) => {
                error!("Error listening on universe {universe}: {e}");
            }
        }
    }

    fn render_ui(&self, frame: &mut Frame) {
        let instructions = Line::from(vec![
            " Select Universe ".into(),
            "<U>".blue().bold(),
            " Confirm ".into(),
            "<Enter>".blue().bold(),
            " Cancel ".into(),
            "<Esc>".blue().bold(),
            " Quit ".into(),
            "<Q> ".blue().bold(),
        ]);

        let (data_rect, info_rect) = render_chrome(
            frame,
            "sACN Merge Receiver Example",
            instructions,
            &self.log,
        );

        let levels = self.merged_valid.then_some(&self.merged_levels);
        render_level_grid(frame, data_rect, "Merged Levels", levels);
        self.render_info_block(frame, info_rect);

        if let AppMode::UniversePicker(picker) = &self.mode {
            picker.render(frame, "Select the universe to listen on");
        }
    }

    fn render_info_block(&self, frame: &mut Frame, area: Rect) {
        let status = if !self.universe_active {
            Line::from("waiting for data".dim())
        } else if self.sampling {
            Line::from("sampling".yellow())
        } else {
            Line::from("active".green())
        };

        let mut lines = vec![
            Line::from(format!("Universe: {}", self.universe)),
            status,
            Line::from(""),
            Line::from(format!("Sources ({})", self.active_sources.len()).bold()),
        ];
        if self.active_sources.is_empty() {
            lines.push(Line::from("none".dim()));
        } else {
            for name in &self.active_sources {
                lines.push(Line::from(format!("- {name}")));
            }
        }

        let info = Paragraph::new(lines).block(Block::bordered().border_set(border::THICK));
        frame.render_widget(info, area);
    }
}
