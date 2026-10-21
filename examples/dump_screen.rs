//! Render a queuetop screen with the real UI code from the fake Slurm tools
//! into a cell dump for docs/render_screen.py.
//!
//!   cargo run -q -p queuetop --example dump_screen -- main  | python3 docs/render_screen.py docs/screenshot.png

use std::io::Write;
use std::path::Path;

use hisstop::events::Hooks;
use queuetop::app::App;
use queuetop::monitor::{Monitor, Options};
use queuetop::slurm::Tools;
use queuetop::ui::Key;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

fn colour(c: Color) -> &'static str {
    match c {
        Color::Yellow => "yellow",
        Color::Green => "green",
        Color::Red => "red",
        Color::Cyan => "cyan",
        _ => "default",
    }
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "main".to_string());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let now = queuetop::slurm::parse_slurm_time("2026-10-20T17:57:00").unwrap();
    std::env::set_var("HISSTOP_NOW", format!("{}", now));
    let opts = Options { squeue_interval: 0.0, sstat_interval: 0.0, sacct_interval: 0.0, ..Default::default() };
    let mut mon = Monitor::new("mike", "discovery", Tools::from_dir(&root.join("tests/fake")), opts);
    mon.wait_all(Some(10.0));
    let mut app = App::new(mon, Hooks::default(), true);
    if which == "popup" {
        app.handle_key(Key::Enter);
        app.mon.wait_all(Some(10.0));
    }
    let (w, h) = (150u16, 30u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.draw(f.buffer_mut())).unwrap();
    let buf = term.backend().buffer();
    let mut out = std::io::stdout().lock();
    for y in 0..h {
        for x in 0..w {
            let c = &buf[(x, y)];
            if c.symbol() == " " && c.fg == Color::Reset && !c.modifier.contains(Modifier::REVERSED) {
                continue;
            }
            let m = c.modifier;
            writeln!(out, "{{\"x\":{},\"y\":{},\"s\":{},\"fg\":\"{}\",\"bold\":{},\"dim\":{},\"rev\":{}}}", x, y, serde_json::to_string(c.symbol()).unwrap(), colour(c.fg), m.contains(Modifier::BOLD), m.contains(Modifier::DIM), m.contains(Modifier::REVERSED)).unwrap();
        }
    }
}
