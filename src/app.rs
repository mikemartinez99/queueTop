//! The event loop, event hooks and scancel requests.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::Terminal;

use crate::events::{describe_all, Event, Hooks};
use crate::monitor::Monitor;
use crate::ui::{map_key, map_mouse, Key, Request, Ui};

pub struct App {
    pub mon: Monitor,
    pub ui: Ui,
    pub hooks: Hooks,
    /// react to clicks and the scroll wheel
    pub mouse: bool,
    seen_final: HashSet<String>,
    primed: bool,
}

impl App {
    pub fn new(mon: Monitor, hooks: Hooks, allow_cancel: bool) -> App {
        let mut ui = Ui::new();
        ui.allow_cancel = allow_cancel;
        App { mon, ui, hooks, mouse: true, seen_final: HashSet::new(), primed: false }
    }

    /// Poll, keep an open popup fresh, and turn newly finished jobs into
    /// notices and hooks. The first poll only records what is already there,
    /// so starting up does not replay the day's history.
    pub fn refresh(&mut self) {
        self.mon.refresh();
        if let Some(d) = &self.ui.detail {
            let id = d.jobid.clone();
            self.mon.request_details(&id);
            if d.show_script {
                self.mon.request_script(&id);
            }
        }
        if self.mon.shared().hist_time.is_none() {
            return;
        }
        let mut events = Vec::new();
        for h in &self.mon.history() {
            if self.seen_final.insert(h.jobid.clone()) && self.primed {
                events.push(Event { jobid: h.jobid.clone(), name: h.name.clone(), state: h.short(), bad: h.is_bad() });
            }
        }
        self.primed = true;
        if events.is_empty() {
            return;
        }
        if events.len() <= 3 {
            for ev in &events {
                self.ui.notify(ev.describe(), if ev.bad { "failed" } else { "done" });
            }
        } else {
            let bad = events.iter().any(|e| e.bad);
            self.ui.notify(describe_all(&events), if bad { "failed" } else { "done" });
        }
        if self.hooks.active() {
            if let Err(e) = self.hooks.fire(&events) {
                self.ui.notify(e, "failed");
            }
        }
    }

    pub fn process_requests(&mut self) {
        for req in self.ui.take_requests() {
            match req {
                Request::Cancel { jobid } => match self.mon.cancel(&jobid) {
                    Ok(msg) => self.ui.notify(msg, "done"),
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
    let mouse = app.mouse;
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    if mouse {
        let _ = io::stdout().execute(EnableMouseCapture);
    }
    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        orig_hook(info);
    }));
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;
    let res = event_loop(&mut terminal, app, interval);
    let _ = terminal.show_cursor();
    if mouse {
        let _ = io::stdout().execute(DisableMouseCapture);
    }
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
                TermEvent::Mouse(m) => {
                    if let Some(m) = map_mouse(m) {
                        app.ui.handle_mouse(m, &mut app.mon);
                        break;
                    }
                }
                TermEvent::Resize(_, _) => break,
                _ => {}
            }
        }
    }
}
