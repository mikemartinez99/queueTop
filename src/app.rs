//! The event loop, event hooks and scancel requests.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as TermEvent};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::Terminal;

use hisstop::events::{Event, Hooks};

use crate::monitor::Monitor;
use crate::ui::{map_key, Key, Request, Ui};

pub struct App {
    pub mon: Monitor,
    pub ui: Ui,
    pub hooks: Hooks,
    seen_final: HashSet<String>,
    primed: bool,
}

impl App {
    pub fn new(mon: Monitor, hooks: Hooks, allow_cancel: bool) -> App {
        let mut ui = Ui::new();
        ui.allow_cancel = allow_cancel;
        App { mon, ui, hooks, seen_final: HashSet::new(), primed: false }
    }

    /// Poll, and turn newly finished jobs into notices and hooks.
    pub fn refresh(&mut self) {
        self.mon.refresh();
        let hist = self.mon.history();
        if self.mon.shared().hist_time.is_none() {
            return;
        }
        let mut events = Vec::new();
        for h in &hist {
            if self.seen_final.insert(h.row.jobid.clone()) && self.primed {
                if h.row.is_bad() {
                    events.push((
                        Event::JobFailed { jobid: h.row.jobid.clone(), rule: h.name.clone(), wildcards: String::new(), ext_id: Some(h.row.jobid.clone()), log: String::new() },
                        format!("job {} ({}) ended: {}", h.row.jobid, h.name, h.row.short()),
                        "failed",
                    ));
                } else {
                    events.push((Event::RunFinished, format!("job {} ({}) completed", h.row.jobid, h.name), "running"));
                }
            }
        }
        self.primed = true;
        let status = format!("{} running", self.mon.jobs().iter().filter(|j| j.is_running()).count());
        for (ev, text, colour) in events {
            self.ui.notify(text, colour);
            if self.hooks.active() {
                if let Err(e) = self.hooks.fire(&ev, std::path::Path::new("."), &status) {
                    self.ui.notify(e, "failed");
                }
            }
        }
        if let Some(d) = &self.ui.detail {
            let id = d.jobid.clone();
            self.mon.request_details(&id);
        }
    }

    pub fn process_requests(&mut self) {
        for req in self.ui.take_requests() {
            match req {
                Request::Cancel { jobid } => match self.mon.cancel(&jobid) {
                    Ok(msg) => self.ui.notify(msg, "running"),
                    Err(e) => self.ui.notify(e, "failed"),
                },
            }
        }
    }

    pub fn draw(&mut self, buf: &mut Buffer) {
        self.ui.draw(&self.mon, buf);
    }

    pub fn handle_key(&mut self, key: Key) -> bool {
        self.ui.handle_key(key, &mut self.mon)
    }
}

pub fn run(app: App, interval: f64) -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        orig_hook(info);
    }));
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;
    let res = event_loop(&mut terminal, app, interval);
    let _ = terminal.show_cursor();
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
    res
}

fn event_loop<B: Backend>(terminal: &mut Terminal<B>, mut app: App, interval: f64) -> io::Result<()> {
    let tick = Duration::from_secs_f64(interval.max(0.05));
    loop {
        app.refresh();
        app.process_requests();
        terminal.draw(|f| app.draw(f.buffer_mut()))?;
        let deadline = Instant::now() + tick;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if !event::poll(deadline - now)? {
                break;
            }
            match event::read()? {
                TermEvent::Key(k) => {
                    if let Some(key) = map_key(k) {
                        if app.handle_key(key) {
                            return Ok(());
                        }
                        break;
                    }
                }
                TermEvent::Resize(_, _) => break,
                _ => {}
            }
        }
    }
}
