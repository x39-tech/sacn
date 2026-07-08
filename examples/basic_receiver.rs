//! A terminal sACN basic receiver.
//!
//! Listens on a universe and renders its DMX levels live in a TUI; press `u` to
//! switch universes. Run it with `cargo run --example basic_receiver`.

#[path = "common/tui.rs"]
mod common;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
};

use common::{
    AppLogBuf, Event, PickerOutcome, SLOTS, UniversePicker, display_cid, init_tui_logging,
    is_quit_event, render_chrome, render_level_grid, valid_universe,
};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent};
use ratatui::{
    prelude::*,
    symbols::border,
    widgets::{Block, Paragraph},
};
use sacn::tokio::BasicReceiver;
use sacn::{BasicReceiverEvent, Cid, ReceiverConfig};
use tracing::{error, info, warn};

const INITIAL_UNIVERSE: u16 = 1;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let receiver = BasicReceiver::bind(ReceiverConfig::new()).await?;

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

    info!("sACN receiver created");

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
    receiver: BasicReceiver,
    universe: u16,
    universe_data: [u8; SLOTS],
    universe_data_valid: bool,
    universe_active: bool,
    sampling: bool,
    mode: AppMode,
    source_names: HashMap<Cid, String>,
}

impl App {
    async fn new(
        mut receiver: BasicReceiver,
        initial_universe: u16,
        log: Arc<Mutex<AppLogBuf>>,
    ) -> Self {
        if let Some(universe) = valid_universe(initial_universe)
            && let Err(e) = receiver.listen(universe).await
        {
            error!("Listening on universe {initial_universe} failed: {e}");
        }

        Self {
            log,
            receiver,
            universe: initial_universe,
            universe_data: [0; SLOTS],
            universe_data_valid: false,
            universe_active: false,
            sampling: false,
            mode: AppMode::Normal,
            source_names: HashMap::new(),
        }
    }

    async fn process_sacn(&mut self) {
        let Some(event) = self.receiver.next_event().await else {
            return;
        };
        match event {
            BasicReceiverEvent::UniverseData(data) => {
                if data.universe.get() == self.universe {
                    self.source_names
                        .insert(data.source.cid, data.source.name.clone());
                    if data.start_code == 0x00 {
                        let num_slots = data.values.len().min(self.universe_data.len());
                        self.universe_data[..num_slots].copy_from_slice(&data.values[..num_slots]);
                        self.universe_data[num_slots..].fill(0);
                        self.universe_data_valid = true;
                        self.universe_active = true;
                        self.sampling = data.is_sampling;
                    }
                }
            }
            BasicReceiverEvent::SamplingEnded { universe } if universe.get() == self.universe => {
                self.sampling = false;
            }
            BasicReceiverEvent::SourcesLost { universe, sources }
                if universe.get() == self.universe =>
            {
                for source in sources {
                    let name = self
                        .source_names
                        .remove(&source.cid)
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| display_cid(source.cid.as_bytes()));
                    warn!("Source {name} lost on universe {universe}");
                }
                self.universe_active = false;
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
                    if let Some(universe) = universe
                        && universe != self.universe
                    {
                        self.switch_universe(universe).await;
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
        if let Some(old_universe) = valid_universe(self.universe)
            && let Err(e) = self.receiver.stop_listening(old_universe).await
        {
            warn!(
                "Error stopping listening on universe {}: {e}",
                self.universe
            );
        }

        match self.receiver.listen(new_universe).await {
            Ok(()) => {
                info!("Now listening on universe {universe}");
                self.universe_data_valid = false;
                self.universe_active = false;
                self.sampling = false;
                self.universe = universe;
                self.source_names.clear();
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

        let (data_rect, info_rect) =
            render_chrome(frame, "sACN Receiver Example", instructions, &self.log);

        let levels = self.universe_data_valid.then_some(&self.universe_data);
        render_level_grid(frame, data_rect, "Universe Data", levels);
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
        let info = Paragraph::new(vec![
            Line::from(format!("Universe: {}", self.universe)),
            status,
        ])
        .block(Block::bordered().border_set(border::THICK));
        frame.render_widget(info, area);
    }
}
