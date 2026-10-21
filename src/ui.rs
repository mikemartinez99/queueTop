//! The screens: queue + recent history, the job popup, the efficiency view,
//! help; and the keys and mouse that drive them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, Widget};

use crate::clock;
use crate::model::{cpu_pct, cpu_secs, group_arrays, maxrss, mem_pct, state_colour};
use crate::monitor::{Monitor, Snap};
use crate::slurm::{jobkey, HistJob, QueueJob};
use crate::tail::{display_path, is_problem_line, TailCache};
use crate::text::{clip, coloured_table, efficiency_table, errors, fmt_clock, fmt_dur, fmt_mem, header_lines, hist_rows, pad, pct, queue_rows, HIST_HEADERS, QUEUE_HEADERS};
use crate::VERSION;

pub const PAD: u16 = 2;
const NOTICE_SECS: f64 = 20.0;
const TAIL_BYTES: u64 = 256 * 1024;
/// two clicks on the same row within this open it
const DOUBLE_CLICK_SECS: f64 = 0.4;
/// cleared finished jobs are remembered this long
const CLEARED_KEEP: f64 = 7.0 * 86400.0;

// ------------------------------------------------------------------ colours, keys, mouse, drawing primitives

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Colour {
    Pending,
    Running,
    Failed,
    Done,
    Head,
    Dim,
    Title,
    Plain,
}

/// pending = yellow, running and done = green, failed = red; default colours
/// keep the terminal background.
pub fn style(c: Colour) -> Style {
    match c {
        Colour::Pending => Style::default().fg(Color::Yellow),
        Colour::Running | Colour::Done => Style::default().fg(Color::Green),
        Colour::Failed => Style::default().fg(Color::Red),
        Colour::Head => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        Colour::Dim => Style::default().add_modifier(Modifier::DIM),
        Colour::Title => Style::default().add_modifier(Modifier::BOLD),
        Colour::Plain => Style::default(),
    }
}

/// A colour name from the model ("running", "failed", "alert" ...) to a Colour.
pub fn colour_named(name: &str) -> Colour {
    match name {
        "pending" => Colour::Pending,
        "running" => Colour::Running,
        "failed" | "alert" => Colour::Failed,
        "done" => Colour::Done,
        "dim" => Colour::Dim,
        _ => Colour::Plain,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    PageUp,
    PageDown,
    Enter,
    Escape,
    Backspace,
}

pub fn map_key(k: KeyEvent) -> Option<Key> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C')) {
        return Some(Key::Char('q'));
    }
    match k.code {
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::PageUp => Some(Key::PageUp),
        KeyCode::PageDown => Some(Key::PageDown),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Escape),
        KeyCode::Backspace => Some(Key::Backspace),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mouse {
    /// left button down at (x, y)
    Click(i32, i32),
    /// wheel: negative up, positive down
    Scroll(i32),
}

pub fn map_mouse(m: MouseEvent) -> Option<Mouse> {
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => Some(Mouse::Click(m.column as i32, m.row as i32)),
        MouseEventKind::ScrollUp => Some(Mouse::Scroll(-3)),
        MouseEventKind::ScrollDown => Some(Mouse::Scroll(3)),
        _ => None,
    }
}

/// Write `s` at (y, x), clipped to the buffer width; off-screen writes are dropped.
pub fn put(buf: &mut Buffer, y: i32, x: i32, s: &str, style: Style) {
    let area = buf.area;
    let (h, w) = (area.height as i32, area.width as i32);
    if y < 0 || y >= h || x < 0 || x >= w {
        return;
    }
    let s = clip(s, (w - x) as i64);
    if s.is_empty() {
        return;
    }
    buf.set_stringn(area.x + x as u16, area.y + y as u16, &s, (w - x) as usize, style);
}

/// Greedy word wrap on spaces, hard-splitting words longer than `width`.
pub fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for seg in s.split('\n') {
        let mut line = String::new();
        let mut line_len = 0usize;
        for word in seg.split(' ') {
            let wlen = word.chars().count();
            if line_len > 0 && line_len + 1 + wlen > width {
                out.push(std::mem::take(&mut line));
                line_len = 0;
            }
            if wlen > width {
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(width) {
                    if line_len > 0 {
                        out.push(std::mem::take(&mut line));
                    }
                    line = chunk.iter().collect();
                    line_len = chunk.len();
                }
                continue;
            }
            if line_len > 0 {
                line.push(' ');
                line_len += 1;
            }
            line.push_str(word);
            line_len += wlen;
        }
        out.push(line);
    }
    out
}

// ------------------------------------------------------------------ state

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    State,
    Time,
    Cpu,
    Mem,
    Name,
    Priority,
}

impl Sort {
    pub fn next(self) -> Sort {
        match self {
            Sort::State => Sort::Time,
            Sort::Time => Sort::Cpu,
            Sort::Cpu => Sort::Mem,
            Sort::Mem => Sort::Name,
            Sort::Name => Sort::Priority,
            Sort::Priority => Sort::State,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::State => "state",
            Sort::Time => "time",
            Sort::Cpu => "cpu%",
            Sort::Mem => "mem%",
            Sort::Name => "name",
            Sort::Priority => "priority",
        }
    }
}

/// Order of the finished list; `s` cycles it while that list has the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HistSort {
    /// most recently ended first
    #[default]
    End,
    Name,
    /// ended badly first, then by state
    State,
    Elapsed,
    Mem,
}

impl HistSort {
    pub fn next(self) -> HistSort {
        match self {
            HistSort::End => HistSort::Name,
            HistSort::Name => HistSort::State,
            HistSort::State => HistSort::Elapsed,
            HistSort::Elapsed => HistSort::Mem,
            HistSort::Mem => HistSort::End,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HistSort::End => "ended",
            HistSort::Name => "name",
            HistSort::State => "state",
            HistSort::Elapsed => "elapsed",
            HistSort::Mem => "mem%",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub at: f64,
    pub text: String,
    pub colour: &'static str,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Cancel { jobid: String },
}

#[derive(Clone, Debug)]
pub struct Detail {
    pub jobid: String,
    pub log_idx: usize,
    /// lines hidden below the bottom of the pane (0 = following the end)
    pub tail_scroll: usize,
    pub tail_room: usize,
    pub tail_len: usize,
    pub log_count: usize,
    /// show the batch script instead of the output file
    pub show_script: bool,
    /// start the next script view at its top
    script_top: bool,
}

/// Where a list's rows were drawn, for clicks.
#[derive(Clone, Copy, Debug)]
struct Hit {
    hist: bool,
    y0: i32,
    rows: usize,
    scroll: usize,
}

#[derive(Debug)]
pub struct Ui {
    pub cursor: usize,
    pub scroll: usize,
    /// the cursor moves over finished jobs instead of the queue
    pub focus_hist: bool,
    /// the FINISHED section is folded to its title line (`c`)
    pub hide_hist: bool,
    /// finished jobs cleared from the screen with `C`: jobid -> when it ended
    pub dismissed: HashMap<String, f64>,
    /// where cleared jobs are remembered between runs (None: not remembered)
    pub cleared_file: Option<PathBuf>,
    /// array tasks fold into one row per array (`a`)
    pub collapse_arrays: bool,
    pub filter: String,
    pub filter_edit: bool,
    pub sort: Sort,
    pub hist_sort: HistSort,
    pub show_help: bool,
    pub show_eff: bool,
    pub detail: Option<Detail>,
    pub confirm: Option<(String, String)>,
    pub allow_cancel: bool,
    pub notices: Vec<Notice>,
    pub list_len: usize,
    pub list_room: usize,
    requests: Vec<Request>,
    hits: Vec<Hit>,
    last_click: Option<(f64, bool, usize)>,
    tail_cache: TailCache,
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

fn matches(hay: &str, needle: &str) -> bool {
    needle.is_empty() || hay.to_lowercase().contains(&needle.to_lowercase())
}

fn desc(a: Option<f64>, b: Option<f64>) -> std::cmp::Ordering {
    b.unwrap_or(-1.0).partial_cmp(&a.unwrap_or(-1.0)).unwrap_or(std::cmp::Ordering::Equal)
}

impl Ui {
    pub fn new() -> Ui {
        Ui {
            cursor: 0,
            scroll: 0,
            focus_hist: false,
            hide_hist: false,
            dismissed: HashMap::new(),
            cleared_file: None,
            collapse_arrays: true,
            filter: String::new(),
            filter_edit: false,
            sort: Sort::State,
            hist_sort: HistSort::End,
            show_help: false,
            show_eff: false,
            detail: None,
            confirm: None,
            allow_cancel: true,
            notices: Vec::new(),
            list_len: 0,
            list_room: 1,
            requests: Vec::new(),
            hits: Vec::new(),
            last_click: None,
            tail_cache: TailCache::default(),
        }
    }

    pub fn notify(&mut self, text: impl Into<String>, colour: &'static str) {
        self.notices.push(Notice { at: clock::now(), text: text.into(), colour });
        if self.notices.len() > 50 {
            self.notices.remove(0);
        }
    }

    pub fn current_notice(&self) -> Option<&Notice> {
        let now = clock::now();
        self.notices.iter().rev().find(|n| now - n.at < NOTICE_SECS)
    }

    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// Read the cleared jobs back from `cleared_file`.
    pub fn load_cleared(&mut self) {
        let Some(p) = &self.cleared_file else { return };
        let Ok(text) = std::fs::read_to_string(p) else { return };
        for line in text.lines() {
            if let Some((id, end)) = line.split_once('\t') {
                if let Ok(end) = end.trim().parse::<f64>() {
                    self.dismissed.insert(id.to_string(), end);
                }
            }
        }
    }

    /// Write the cleared jobs to `cleared_file`, forgetting old ones.
    pub fn save_cleared(&mut self) {
        let Some(p) = &self.cleared_file else { return };
        let now = clock::now();
        self.dismissed.retain(|_, end| now - *end < CLEARED_KEEP);
        let mut lines: Vec<String> = self.dismissed.iter().map(|(id, end)| format!("{}\t{:.0}", id, end)).collect();
        lines.sort();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, lines.join("\n") + "\n");
    }

    /// Queue jobs after the filter, the sort and the array folding.
    pub fn visible_jobs(&self, snap: &Snap) -> Vec<QueueJob> {
        let mut jobs = snap.jobs.clone();
        jobs.retain(|j| [j.jobid.as_str(), j.name.as_str(), j.partition.as_str(), j.state.as_str(), j.nodelist.as_str(), j.reason.as_str()].iter().any(|f| matches(f, &self.filter)));
        if self.collapse_arrays {
            jobs = group_arrays(jobs);
        }
        let live = |j: &QueueJob| crate::text::live_use(snap, j);
        match self.sort {
            Sort::State => {}
            Sort::Time => jobs.sort_by(|a, b| desc(a.time_used.map(|t| t as f64), b.time_used.map(|t| t as f64))),
            Sort::Cpu => jobs.sort_by_cached_key(|j| std::cmp::Reverse(live(j).0.map(|c| (c * 1e6) as i64).unwrap_or(-1))),
            Sort::Mem => jobs.sort_by_cached_key(|j| std::cmp::Reverse(live(j).1.map(|m| (m * 1e6) as i64).unwrap_or(-1))),
            Sort::Name => jobs.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| jobkey(&a.jobid).cmp(&jobkey(&b.jobid)))),
            Sort::Priority => jobs.sort_by_key(|j| std::cmp::Reverse(j.priority)),
        }
        jobs
    }

    /// Finished jobs after the filter and the sort, minus the ones cleared with `C`.
    pub fn visible_hist(&self, snap: &Snap) -> Vec<HistJob> {
        let mut h = snap.hist.clone();
        h.retain(|h| !self.dismissed.contains_key(&h.jobid) && [h.jobid.as_str(), h.name.as_str(), h.state.as_str(), h.partition.as_str()].iter().any(|f| matches(f, &self.filter)));
        match self.hist_sort {
            HistSort::End => {}
            HistSort::Name => h.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| jobkey(&b.jobid).cmp(&jobkey(&a.jobid)))),
            HistSort::State => h.sort_by(|a, b| b.is_bad().cmp(&a.is_bad()).then_with(|| a.short().cmp(&b.short()))),
            HistSort::Elapsed => h.sort_by(|a, b| desc(a.elapsed.map(|t| t as f64), b.elapsed.map(|t| t as f64))),
            HistSort::Mem => h.sort_by(|a, b| desc(a.mem_use(), b.mem_use())),
        }
        h
    }

    /// The job id and name under the cursor, in whichever list has focus.
    fn cursor_job(&self, snap: &Snap) -> Option<(String, String)> {
        if self.focus_hist {
            let h = self.visible_hist(snap);
            h.get(self.cursor.min(h.len().saturating_sub(1))).filter(|_| !h.is_empty()).map(|h| (h.jobid.clone(), h.name.clone()))
        } else {
            let j = self.visible_jobs(snap);
            j.get(self.cursor.min(j.len().saturating_sub(1))).filter(|_| !j.is_empty()).map(|j| (j.jobid.clone(), j.name.clone()))
        }
    }

    fn section(&self, buf: &mut Buffer, y: i32, title: &str, hint: &str) {
        put(buf, y, PAD as i32, title, style(Colour::Title));
        if !hint.is_empty() {
            put(buf, y, PAD as i32 + title.chars().count() as i32 + 3, hint, style(Colour::Dim));
        }
    }

    fn list_hint(&self, shown: usize, total: usize, focused: bool) -> String {
        let mut s = if shown == total { format!("{}", total) } else { format!("{} of {}", shown, total) };
        if !self.filter.is_empty() {
            s.push_str(&format!("   filter '{}'", self.filter));
        }
        if focused && !self.focus_hist && self.sort != Sort::State {
            s.push_str(&format!("   by {}", self.sort.label()));
        }
        if focused && self.focus_hist {
            if self.hist_sort != HistSort::End {
                s.push_str(&format!("   by {}", self.hist_sort.label()));
            }
            s.push_str("   f back to queue");
        }
        s
    }

    // ---- drawing

    pub fn draw(&mut self, mon: &Monitor, buf: &mut Buffer) {
        let (h, w) = (buf.area.height as i32, buf.area.width as i32);
        let x = PAD as i32;
        self.hits.clear();
        if h < 10 || w < 60 {
            put(buf, 0, 0, "terminal too small (need 60x10)", style(Colour::Plain));
            return;
        }
        let snap = mon.snapshot();
        let (l1, l2) = header_lines(mon, &snap, (w - x) as i64);
        put(buf, 0, x, &l1, style(Colour::Head));
        put(buf, 1, x, &l2, style(Colour::Dim));
        let mut y = 2;
        let errs = errors(mon);
        let notice = self.current_notice().cloned();
        if !errs.is_empty() || notice.is_some() {
            let mut xx = x;
            if !errs.is_empty() {
                let t = errs.join("  /  ");
                put(buf, y, x, &t, style(Colour::Pending));
                xx += t.chars().count() as i32;
            }
            if let Some(n) = notice {
                let sep = if xx > x { "  /  " } else { "" };
                put(buf, y, xx, &format!("{}> {}", sep, n.text), style(colour_named(n.colour)).add_modifier(Modifier::BOLD));
            }
            y += 1;
        }
        y += 1;
        if self.show_help {
            self.draw_help(buf, y);
            return;
        }
        if self.show_eff {
            self.draw_efficiency(mon, &snap, buf, y, w, h);
            return;
        }

        let jobs = self.visible_jobs(&snap);
        let hist = self.visible_hist(&snap);
        let footer_y = h - 1;
        let hist_h = if snap.hist.is_empty() {
            0
        } else if self.hide_hist {
            2
        } else {
            let fit = ((hist.len() as i32 + 3).min(((footer_y - y) * 2 / 5).max(5))).min(footer_y - y - 6);
            if self.focus_hist { (footer_y - y - 6).max(fit) } else { fit.max(0) }
        };
        let queue_h = footer_y - y - hist_h;

        let (lines, colours) = coloured_table(queue_rows(&snap, &jobs), QUEUE_HEADERS, (w - 2 * x) as i64);
        let mut hint = self.list_hint(jobs.len(), snap.jobs.len(), !self.focus_hist);
        if jobs.iter().any(|j| j.is_group()) {
            hint.push_str("   arrays folded, a to expand");
        }
        let empty = snap.jobs.is_empty().then(|| if mon.shared().jobs_time.is_none() { "waiting for squeue".to_string() } else { format!("no jobs in the queue for {}", mon.user) });
        self.draw_list(buf, y, queue_h, w, "QUEUE", &hint, &lines, &colours, false, false, &empty);
        if self.hide_hist && hist_h > 0 {
            self.section(buf, y + queue_h, "FINISHED", &format!("{} collapsed   c to show", hist.len()));
        } else if hist_h > 0 {
            let (lines, colours) = coloured_table(hist_rows(&hist), HIST_HEADERS, (w - 2 * x) as i64);
            let hint = format!("{}   f {}", self.list_hint(hist.len(), snap.hist.len(), self.focus_hist), if self.focus_hist { "" } else { "to move through them" });
            let empty = Some(if snap.hist.len() > hist.len() && self.filter.is_empty() { "all cleared".to_string() } else { "none".to_string() });
            self.draw_list(buf, y + queue_h, hist_h, w, "FINISHED", hint.trim_end(), &lines, &colours, true, true, &empty);
        }

        if let Some((id, name)) = &self.confirm {
            put(buf, footer_y, x, &format!("scancel {} ({})?   y confirms, any other key keeps it", id, name), style(Colour::Failed).add_modifier(Modifier::BOLD));
        } else if self.filter_edit {
            put(buf, footer_y, x, &format!("filter: {}_", self.filter), style(Colour::Pending).add_modifier(Modifier::BOLD));
            put(buf, footer_y, x + 10 + self.filter.chars().count() as i32, "   enter keep   esc clear", style(Colour::Dim));
        } else {
            let mut keys = vec!["q quit", "j/k move", "enter job", "/ filter", "s sort", "f finished", "c collapse", "C clear", "a arrays", "e efficiency", "r refresh", "? help"];
            if self.allow_cancel {
                keys.insert(3, "x scancel");
            }
            put(buf, footer_y, x, &keys.join("   "), style(Colour::Dim));
        }
        if self.detail.is_some() {
            self.draw_detail(mon, &snap, buf);
        }
    }

    /// A titled table. With `whole_row` the row colour covers the line (the
    /// finished list); without it only the glyph in the first cell is
    /// coloured, blinking for a running job, and the text stays plain.
    #[allow(clippy::too_many_arguments)]
    fn draw_list(&mut self, buf: &mut Buffer, y: i32, hgt: i32, w: i32, title: &str, hint: &str, lines: &[String], colours: &[&'static str], hist: bool, whole_row: bool, empty_msg: &Option<String>) {
        if hgt < 3 {
            return;
        }
        let focused = hist == self.focus_hist;
        let x = PAD as i32;
        self.section(buf, y, title, hint);
        let body = &lines[1..];
        let room = (hgt - 3).max(0) as usize;
        let scroll = if focused {
            self.list_len = body.len();
            self.list_room = room.max(1);
            if body.is_empty() {
                self.cursor = 0;
                self.scroll = 0;
            } else {
                self.cursor = self.cursor.min(body.len() - 1);
                if self.cursor < self.scroll {
                    self.scroll = self.cursor;
                }
                if room > 0 && self.cursor >= self.scroll + room {
                    self.scroll = self.cursor + 1 - room;
                }
                self.scroll = self.scroll.min(body.len().saturating_sub(room));
            }
            self.scroll
        } else {
            0
        };
        put(buf, y + 1, x, &lines[0], style(Colour::Dim));
        let shown = body.len().saturating_sub(scroll).min(room);
        if shown > 0 {
            self.hits.push(Hit { hist, y0: y + 2, rows: shown, scroll });
        }
        for (i, line) in body.iter().skip(scroll).take(room).enumerate() {
            let name = colours.get(scroll + i).copied().unwrap_or("dim");
            let cursor = if focused && scroll + i == self.cursor { Modifier::REVERSED | Modifier::BOLD } else { Modifier::empty() };
            let yy = y + 2 + i as i32;
            if whole_row {
                put(buf, yy, x, line, style(colour_named(name)).add_modifier(cursor));
            } else {
                let mut chars = line.chars();
                let glyph: String = chars.next().into_iter().collect();
                let rest: String = chars.collect();
                let mut gs = style(colour_named(name)).add_modifier(cursor);
                if name == "running" || name == "alert" {
                    gs = gs.add_modifier(Modifier::SLOW_BLINK);
                }
                put(buf, yy, x, &glyph, gs);
                put(buf, yy, x + 1, &rest, style(Colour::Plain).add_modifier(cursor));
            }
        }
        if body.is_empty() {
            put(buf, y + 2, x, empty_msg.as_deref().unwrap_or("none"), style(Colour::Dim));
        }
        if body.len() > room && room > 0 {
            put(buf, y + 1 + room as i32, w - 18, &format!("{}-{} of {}", scroll + 1, body.len().min(scroll + room), body.len()), style(Colour::Dim));
        }
    }

    fn draw_efficiency(&self, mon: &Monitor, snap: &Snap, buf: &mut Buffer, y: i32, w: i32, h: i32) {
        let x = PAD as i32;
        let (lines, colours) = efficiency_table(&snap.hist, (w - 2 * x) as i64);
        let n = lines.len().saturating_sub(1);
        self.section(buf, y, "EFFICIENCY", &format!("finished jobs grouped by name, {}   e back", mon.opts.since.replace("now-", "last ")));
        let mut yy = y + 2;
        if n == 0 {
            let sh = mon.shared();
            let why = if !sh.use_sacct {
                "sacct is off or missing"
            } else if !sh.hist_err.is_empty() {
                sh.hist_err.as_str()
            } else if sh.hist_time.is_none() {
                "waiting for sacct"
            } else {
                "no finished job in the window; widen it with --since"
            };
            put(buf, yy, x, &format!("nothing to show: {}", why), style(Colour::Pending));
        } else {
            for (i, line) in lines.iter().enumerate() {
                if yy >= h - 5 {
                    break;
                }
                let st = if i == 0 { style(Colour::Dim) } else { style(colour_named(colours[i - 1])) };
                put(buf, yy, x, line, st);
                yy += 1;
            }
            yy += 1;
            put(buf, yy, x, "time = max Elapsed / Timelimit   mem = max MaxRSS / ReqMem   cpu = median TotalCPU / (Elapsed x AllocCPUS)", style(Colour::Dim));
            put(buf, yy + 1, x, "next request = the peaks seen plus a quarter, rounded up; after an OOM or TIMEOUT the peak is the old limit, so go higher if that still fails", style(Colour::Dim));
            put(buf, yy + 2, x, "red: OOM / TIMEOUT / NODE_FAIL / failed   yellow: >90% of a limit, or memory over-requested", style(Colour::Dim));
        }
        put(buf, h - 1, x, "q quit   e back   ? help", style(Colour::Dim));
    }

    fn draw_help(&self, buf: &mut Buffer, y: i32) {
        let lines = [
            format!("queuetop {}", VERSION),
            String::new(),
            "  j / k, arrows  move the cursor (J / K, PgUp / PgDn by page); the mouse wheel scrolls, a click selects, a double click opens".to_string(),
            "  Enter          open the job: scontrol details, live CPU and memory, sacct figures, output file tail".to_string(),
            "                 (in the popup: j/k scroll, G follow the end, l stdout/stderr, b the batch script, Esc close)".to_string(),
            "  f              move through the finished jobs instead of the queue / back".to_string(),
            "  c              collapse the finished list to one line / show it again".to_string(),
            "  C              clear the finished list (remembered across runs; jobs that end later still appear)".to_string(),
            "  a              fold array tasks into one row per array / expand them".to_string(),
            "  /              filter by jobid, name, partition, state, node or reason (Esc clears)".to_string(),
            "  s              sort the list under the cursor: queue by state, time, cpu%, mem%, name, priority; finished by ended, name, state, elapsed, mem%".to_string(),
            "  e              efficiency of finished jobs grouped by name, with the request to make next time / back".to_string(),
            "  x              scancel the job under the cursor (or in the popup), after y/n".to_string(),
            "  r              poll everything now".to_string(),
            "  ?              close this help   q quit".to_string(),
            String::new(),
            "Queue markers: * running (green, blinking; red once past 90% of the time limit or memory request), ^ pending (yellow), # stopped (red).".to_string(),
            "Finished rows: green completed, red ended badly; + completed, ! OOM kill, x other failure.".to_string(),
            "cpu% = CPU seconds used / (elapsed x cores), from sstat while running, sacct afterwards.   mem% = MaxRSS / memory requested.".to_string(),
            "Defaults can be set in ~/.config/queuetop.toml, one `option = value` per line using the long option names.".to_string(),
        ];
        for (i, l) in lines.iter().enumerate() {
            put(buf, y + i as i32, PAD as i32, l, style(Colour::Plain));
        }
    }

    /// Output files of a job: StdOut and StdErr from scontrol, resolved against WorkDir.
    pub fn log_candidates(details: Option<&indexmap::IndexMap<String, String>>, job: Option<&QueueJob>) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut wanted: Vec<PathBuf> = Vec::new();
        if let Some(d) = details {
            let wd = d.get("WorkDir").cloned().or_else(|| job.map(|j| j.workdir.clone())).unwrap_or_default();
            for k in ["StdOut", "StdErr"] {
                if let Some(p) = d.get(k) {
                    if !p.is_empty() && p != "(null)" {
                        let pb = PathBuf::from(p);
                        wanted.push(if pb.is_absolute() || wd.is_empty() { pb } else { Path::new(&wd).join(pb) });
                    }
                }
            }
        }
        let mut have = Vec::new();
        let mut missing = Vec::new();
        for p in wanted {
            if have.contains(&p) || missing.contains(&p) {
                continue;
            }
            if p.is_file() {
                have.push(p);
            } else {
                missing.push(p);
            }
        }
        (have, missing)
    }

    /// Rows of the popup.
    fn detail_rows(&self, mon: &Monitor, snap: &Snap, jobid: &str) -> (Vec<(String, String, &'static str)>, String) {
        let now = clock::now();
        let job = snap.jobs.iter().find(|j| j.jobid == jobid);
        let hist = snap.hist_for(jobid);
        let stat = snap.stat_for(jobid);
        let details = mon.details_for(jobid);
        let mut rows: Vec<(String, String, &'static str)> = Vec::new();
        let mut push = |k: &str, v: String, c: &'static str| {
            if !v.is_empty() {
                rows.push((k.to_string(), v, c));
            }
        };
        let name = job.map(|j| j.name.clone()).or_else(|| hist.map(|h| h.name.clone())).unwrap_or_default();
        if let Some(j) = job {
            let mut st = j.state.clone();
            if j.is_pending() && !j.reason.is_empty() && j.reason != "None" {
                st.push_str(&format!(" ({})", j.reason));
            }
            if let Some(t) = j.start.filter(|_| j.is_pending()) {
                if t > now {
                    st.push_str(&format!(" | expected to start in {}", fmt_dur(Some(t - now))));
                }
            }
            push("state", st, state_colour(&j.state));
            push("where", format!("partition {} | account {} | qos {}{}", j.partition, j.account, j.qos, if j.nodelist.is_empty() { String::new() } else { format!(" | node {}", j.nodelist) }), "plain");
            let mut res = format!("{} cpu{} | {} nodes", j.cpus, if j.cpus == 1 { "" } else { "s" }, j.nodes);
            if let Some(m) = j.mem_req {
                res.push_str(&format!(" | {} memory", fmt_mem(m)));
            }
            push("request", res, "plain");
            let mut t = format!("{} of {}", fmt_dur(j.time_used.map(|t| t as f64)), fmt_dur(j.time_limit.map(|t| t as f64)));
            if let Some(u) = j.time_use() {
                t.push_str(&format!(" ({:.0}%)", u * 100.0));
            }
            if let Some(d) = j.deadline().filter(|_| j.is_running()) {
                t.push_str(&format!(" | limit reached at {}", fmt_clock(d)));
            }
            push("time", t, if j.time_use().is_some_and(|u| u > 0.9) { "pending" } else { "plain" });
            if j.is_running() {
                let cpu = cpu_pct(j, stat, hist);
                let rss = maxrss(stat, hist);
                let memp = mem_pct(j, stat, hist);
                let mut u = String::new();
                if let Some(c) = cpu_secs(j, stat, hist) {
                    u.push_str(&format!("cpu {} busy ({} CPU-seconds)", pct(cpu), c as i64));
                }
                if let Some(r) = rss {
                    if !u.is_empty() {
                        u.push_str(" | ");
                    }
                    u.push_str(&format!("MaxRSS {}", fmt_mem(r)));
                    if let Some(m) = memp {
                        u.push_str(&format!(" ({:.0}% of request)", m * 100.0));
                    }
                }
                if u.is_empty() {
                    u = "no usage data yet (sstat answers once the first step runs)".to_string();
                }
                let colour = if memp.is_some_and(|m| m > 0.9) {
                    "failed"
                } else if cpu.is_some_and(|c| c < 0.25 && j.time_used.unwrap_or(0) > 300) {
                    "pending"
                } else {
                    "plain"
                };
                push("usage", u, colour);
            }
            let mut when = Vec::new();
            if let Some(s) = j.submit {
                when.push(format!("submitted {}", fmt_clock(s)));
            }
            if let (Some(s), Some(st)) = (j.submit, j.start) {
                if j.is_running() {
                    when.push(format!("started {} (waited {})", fmt_clock(st), fmt_dur(Some((st - s).max(0.0)))));
                }
            }
            push("when", when.join(" | "), "plain");
            if !j.dependency.is_empty() && j.dependency != "(null)" {
                push("depends", j.dependency.clone(), "pending");
            }
            if !j.array_task.is_empty() && j.array_task != "N/A" {
                push("array", format!("job {} task {}", j.array_job, j.array_task), "plain");
            }
            push("priority", j.priority.to_string(), "plain");
            push("workdir", j.workdir.clone(), "plain");
        }
        if let Some(h) = hist {
            if h.is_final() {
                push("sacct", h.summary(), if h.is_bad() { "failed" } else if h.near_limit() { "pending" } else { "done" });
                let mut when = Vec::new();
                if let Some(s) = h.submit {
                    when.push(format!("submitted {}", fmt_clock(s)));
                }
                if let Some(s) = h.start {
                    when.push(format!("started {}", fmt_clock(s)));
                }
                if let Some(e) = h.end {
                    when.push(format!("ended {}", fmt_clock(e)));
                }
                if let Some(w) = h.wait() {
                    when.push(format!("waited {}", fmt_dur(Some(w.max(0.0)))));
                }
                push("when", when.join(" | "), "plain");
                push("where", format!("partition {}{}", h.partition, if h.nodelist.is_empty() { String::new() } else { format!(" | node {}", h.nodelist) }), "plain");
            }
        }
        if let Some(d) = &details {
            for (k, label) in [("Command", "command"), ("StdOut", "stdout"), ("StdErr", "stderr"), ("Comment", "comment"), ("Features", "features"), ("TRES", "tres")] {
                if let Some(v) = d.get(k) {
                    if !v.is_empty() && v != "(null)" {
                        push(label, v.clone(), "plain");
                    }
                }
            }
        } else if let Some(e) = mon.details_err(jobid) {
            push("scontrol", e, "pending");
        } else {
            push("scontrol", "fetching details ...".to_string(), "dim");
        }
        (rows, name)
    }

    fn draw_detail(&mut self, mon: &Monitor, snap: &Snap, buf: &mut Buffer) {
        let Some(mut d) = self.detail.take() else { return };
        let area = buf.area;
        let pw = (area.width as u32 * 9 / 10).clamp(48, area.width as u32) as u16;
        let ph = (area.height as u32 * 9 / 10).clamp(10, area.height as u32) as u16;
        let rect = Rect::new((area.width - pw) / 2, (area.height - ph) / 2, pw, ph);
        // the terminal's blur: everything behind the popup goes dark grey, with no
        // bold, reverse or blink left to compete with it
        for cell in buf.content.iter_mut() {
            cell.fg = Color::DarkGray;
            cell.bg = Color::Reset;
            cell.modifier = Modifier::DIM;
        }
        Clear.render(rect, buf);
        let (rows, name) = self.detail_rows(mon, snap, &d.jobid);
        let tc = colour_named(rows.first().map(|(_, _, c)| *c).unwrap_or("plain"));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(style(tc))
            .title(format!(" job {} ({}) ", d.jobid, clip(&name, 50)))
            .title_style(style(tc).add_modifier(Modifier::BOLD))
            .title_bottom(" Esc close   j/k scroll   G follow   l stdout/stderr   b script   x scancel ");
        let inner = block.inner(rect);
        block.render(rect, buf);
        if inner.width < 20 || inner.height < 4 {
            self.detail = Some(d);
            return;
        }
        let (ix, iy, iw, ih) = (inner.x as i32, inner.y as i32, inner.width as usize, inner.height as i32);
        let label_w = rows.iter().map(|(k, _, _)| k.chars().count()).max().unwrap_or(8).clamp(8, 12);
        let mut out: Vec<(String, Style)> = Vec::new();
        for (k, v, c) in &rows {
            for (i, line) in wrap_text(v, iw.saturating_sub(label_w + 2).max(10)).into_iter().enumerate() {
                out.push((format!("{:>w$}  {}", if i == 0 { k.as_str() } else { "" }, line, w = label_w), style(colour_named(c))));
            }
        }
        let job = snap.jobs.iter().find(|j| j.jobid == d.jobid);
        let details = mon.details_for(&d.jobid);
        let (logs, missing) = Self::log_candidates(details.as_ref(), job);
        d.log_count = logs.len();
        let details_h = (out.len() as i32).min((ih * 55 / 100).max(3));
        for (i, (line, st)) in out.iter().enumerate().take(details_h as usize) {
            put(buf, iy + i as i32, ix, &pad(line, iw as i64), *st);
        }
        if out.len() as i32 > details_h && details_h > 0 {
            put(buf, iy + details_h - 1, ix + iw as i32 - 10, &format!("+{} more", out.len() as i32 - details_h), style(Colour::Dim));
        }
        let ly = iy + details_h;
        let room = (ih - details_h - 1).max(1) as usize;
        d.tail_room = room;

        // the lower pane: the batch script, or the tail of an output file
        let (mut head, lines, err): (String, Vec<String>, Option<String>) = if d.show_script {
            let head = format!("SCRIPT  scontrol write batch_script {}   b back to output", d.jobid);
            match mon.script_for(&d.jobid) {
                Some(Ok(text)) => (head, text.lines().map(|l| l.to_string()).collect(), None),
                Some(Err(e)) => (head, Vec::new(), Some(e)),
                None => (head, Vec::new(), Some("fetching script ...".to_string())),
            }
        } else if logs.is_empty() {
            put(buf, ly, ix, "OUTPUT", style(Colour::Title));
            put(buf, ly, ix + 8, if details.is_some() { "no output file yet" } else { "waiting for scontrol" }, style(Colour::Dim));
            for (i, m) in missing.iter().enumerate() {
                put(buf, ly + 1 + i as i32, ix + 2, &m.display().to_string(), style(Colour::Dim));
            }
            d.tail_len = 0;
            self.detail = Some(d);
            return;
        } else {
            d.log_idx %= logs.len();
            let path = &logs[d.log_idx];
            let wd = job.map(|j| PathBuf::from(&j.workdir)).unwrap_or_default();
            let mut head = format!("OUTPUT  {}", display_path(&wd, path));
            if logs.len() > 1 {
                head.push_str(&format!("   [{}/{}, l for next]", d.log_idx + 1, logs.len()));
            }
            match self.tail_cache.get(path, TAIL_BYTES) {
                Ok(t) => {
                    head.push_str(&format!("   {}", t.describe()));
                    (head, t.lines.clone(), None)
                }
                Err(e) => (head, Vec::new(), Some(format!("cannot read: {}", e))),
            }
        };
        d.tail_len = lines.len();
        if d.script_top {
            d.tail_scroll = lines.len().saturating_sub(room);
            d.script_top = false;
        }
        d.tail_scroll = d.tail_scroll.min(lines.len().saturating_sub(room));
        if d.tail_scroll > 0 {
            head.push_str(&format!("   ^ {} lines up, G to end", d.tail_scroll));
        }
        put(buf, ly, ix, &clip(&head, iw as i64), style(Colour::Title));
        if let Some(e) = err {
            put(buf, ly + 1, ix, &e, style(if e.starts_with("fetching") { Colour::Dim } else { Colour::Failed }));
        } else if lines.is_empty() {
            put(buf, ly + 1, ix, "(empty)", style(Colour::Dim));
        }
        let end = lines.len() - d.tail_scroll;
        let start = end.saturating_sub(room);
        for (i, line) in lines[start..end].iter().enumerate() {
            let st = if !d.show_script && is_problem_line(line) { style(Colour::Failed) } else { style(Colour::Plain) };
            put(buf, ly + 1 + i as i32, ix, &pad(&line.replace('\t', "    "), iw as i64), st);
        }
        self.detail = Some(d);
    }

    // ---- keys and mouse

    fn ask_cancel(&mut self, snap: &Snap, jobid: &str, name: &str) {
        if !self.allow_cancel {
            self.notify("scancel is off (--no-cancel)", "pending");
            return;
        }
        if jobid.ends_with("_*") {
            self.notify("expand the array (a) to cancel a task", "pending");
        } else if snap.jobs.iter().any(|j| j.jobid == jobid) {
            self.confirm = Some((jobid.to_string(), name.to_string()));
        } else {
            self.notify(format!("job {} is not in the queue any more", jobid), "pending");
        }
    }

    /// A click selects the row under it; a second click on the same row opens it.
    /// The wheel moves the cursor, or scrolls the popup.
    pub fn handle_mouse(&mut self, m: Mouse, mon: &mut Monitor) {
        match m {
            Mouse::Scroll(n) => {
                if let Some(d) = self.detail.as_mut() {
                    let max_up = d.tail_len.saturating_sub(d.tail_room);
                    d.tail_scroll = (d.tail_scroll as i64 - n as i64).clamp(0, max_up as i64) as usize;
                } else if !self.show_help && !self.show_eff && !self.filter_edit && self.confirm.is_none() {
                    self.cursor = (self.cursor as i64 + n as i64).max(0) as usize;
                    if self.list_len > 0 {
                        self.cursor = self.cursor.min(self.list_len - 1);
                    }
                }
            }
            Mouse::Click(_, y) => {
                if self.detail.is_some() || self.show_help || self.show_eff || self.filter_edit || self.confirm.is_some() {
                    return;
                }
                let Some(hit) = self.hits.iter().find(|h| y >= h.y0 && y < h.y0 + h.rows as i32).copied() else { return };
                let row = hit.scroll + (y - hit.y0) as usize;
                if hit.hist != self.focus_hist {
                    self.focus_hist = hit.hist;
                    self.scroll = 0;
                }
                self.cursor = row;
                let now = clock::now();
                let again = self.last_click.is_some_and(|(t, h, r)| h == hit.hist && r == row && now - t < DOUBLE_CLICK_SECS);
                self.last_click = Some((now, hit.hist, row));
                if again {
                    self.handle_key(Key::Enter, mon);
                }
            }
        }
    }

    /// Returns true to quit.
    pub fn handle_key(&mut self, key: Key, mon: &mut Monitor) -> bool {
        if let Some((id, _)) = self.confirm.take() {
            if matches!(key, Key::Char('y') | Key::Char('Y')) {
                self.requests.push(Request::Cancel { jobid: id.clone() });
                self.notify(format!("sending scancel {}", id), "pending");
            } else {
                self.notify(format!("kept job {}", id), "dim");
            }
            return false;
        }
        if let Some(d) = self.detail.as_mut() {
            let page = d.tail_room.saturating_sub(1).max(1);
            let max_up = d.tail_len.saturating_sub(d.tail_room);
            match key {
                Key::Escape | Key::Char('q') | Key::Enter => self.detail = None,
                Key::Char('k') | Key::Up => d.tail_scroll = (d.tail_scroll + 1).min(max_up),
                Key::Char('j') | Key::Down => d.tail_scroll = d.tail_scroll.saturating_sub(1),
                Key::Char('K') | Key::PageUp => d.tail_scroll = (d.tail_scroll + page).min(max_up),
                Key::Char('J') | Key::PageDown => d.tail_scroll = d.tail_scroll.saturating_sub(page),
                Key::Char('G') | Key::Char('g') => d.tail_scroll = 0,
                Key::Char('l') => {
                    d.show_script = false;
                    if d.log_count > 0 {
                        d.log_idx = (d.log_idx + 1) % d.log_count;
                        d.tail_scroll = 0;
                    }
                }
                Key::Char('b') => {
                    d.show_script = !d.show_script;
                    d.script_top = d.show_script;
                    d.tail_scroll = 0;
                    if d.show_script {
                        let id = d.jobid.clone();
                        mon.request_script(&id);
                    }
                }
                Key::Char('x') | Key::Char('X') => {
                    let id = d.jobid.clone();
                    let snap = mon.snapshot();
                    let name = snap.jobs.iter().find(|j| j.jobid == id).map(|j| j.name.clone()).unwrap_or_default();
                    self.ask_cancel(&snap, &id, &name);
                }
                _ => {}
            }
            return false;
        }
        if self.filter_edit {
            match key {
                Key::Char(c) => self.filter.push(c),
                Key::Backspace => {
                    self.filter.pop();
                }
                Key::Enter => self.filter_edit = false,
                Key::Escape => {
                    self.filter.clear();
                    self.filter_edit = false;
                }
                _ => {}
            }
            self.cursor = 0;
            return false;
        }
        let page = self.list_room.saturating_sub(1).max(1);
        match key {
            Key::Char('q') | Key::Char('Q') => return true,
            Key::Char('?') => {
                self.show_help = !self.show_help;
                self.show_eff = false;
            }
            Key::Char('e') => {
                self.show_eff = !self.show_eff;
                self.show_help = false;
            }
            Key::Escape => {
                if !self.filter.is_empty() && !self.show_help && !self.show_eff {
                    self.filter.clear();
                    self.cursor = 0;
                }
                self.show_help = false;
                self.show_eff = false;
            }
            Key::Char('/') => {
                self.filter_edit = true;
                self.show_help = false;
                self.show_eff = false;
            }
            Key::Char('s') => {
                if self.focus_hist {
                    self.hist_sort = self.hist_sort.next();
                } else {
                    self.sort = self.sort.next();
                }
                self.cursor = 0;
            }
            Key::Char('f') => {
                if self.focus_hist {
                    self.focus_hist = false;
                } else if self.visible_hist(&mon.snapshot()).is_empty() {
                    self.notify("no finished jobs to move through", "dim");
                } else {
                    self.focus_hist = true;
                    self.hide_hist = false;
                }
                self.cursor = 0;
                self.scroll = 0;
            }
            Key::Char('c') => {
                self.hide_hist = !self.hide_hist;
                if self.hide_hist && self.focus_hist {
                    self.focus_hist = false;
                    self.cursor = 0;
                    self.scroll = 0;
                }
            }
            Key::Char('C') => {
                let snap = mon.snapshot();
                let now = clock::now();
                let cleared: Vec<(String, f64)> = self.visible_hist(&snap).into_iter().map(|h| (h.jobid, h.end.unwrap_or(now))).collect();
                let n = cleared.len();
                self.dismissed.extend(cleared);
                self.save_cleared();
                self.notify(format!("cleared {} finished job{}", n, if n == 1 { "" } else { "s" }), "dim");
                if self.focus_hist {
                    self.focus_hist = false;
                    self.cursor = 0;
                    self.scroll = 0;
                }
            }
            Key::Char('a') => {
                self.collapse_arrays = !self.collapse_arrays;
                self.cursor = 0;
            }
            Key::Char('r') => {
                mon.refresh();
                self.notify("polling", "dim");
            }
            Key::Char('k') | Key::Up => self.cursor = self.cursor.saturating_sub(1),
            Key::Char('j') | Key::Down => self.cursor += 1,
            Key::Char('K') | Key::PageUp => self.cursor = self.cursor.saturating_sub(page),
            Key::Char('J') | Key::PageDown => self.cursor += page,
            Key::Enter => {
                self.show_help = false;
                self.show_eff = false;
                if let Some((id, _)) = self.cursor_job(&mon.snapshot()) {
                    if id.ends_with("_*") {
                        self.collapse_arrays = false;
                        self.notify("array expanded; a folds it again", "dim");
                    } else {
                        mon.request_details(&id);
                        self.detail = Some(Detail { jobid: id, log_idx: 0, tail_scroll: 0, tail_room: 1, tail_len: 0, log_count: 0, show_script: false, script_top: false });
                    }
                }
            }
            Key::Char('x') | Key::Char('X') => {
                let snap = mon.snapshot();
                if let Some((id, name)) = self.cursor_job(&snap) {
                    self.ask_cancel(&snap, &id, &name);
                }
            }
            _ => {}
        }
        if self.list_len > 0 {
            self.cursor = self.cursor.min(self.list_len - 1);
        }
        false
    }
}
