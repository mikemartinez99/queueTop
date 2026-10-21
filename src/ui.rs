//! The screens: queue + recent history, the job popup, the efficiency view,
//! help; and the keys that drive them.

use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, Widget};

pub use hisstop::ui::{colour_named, map_key, put, style, wrap_text, Colour, Key};

use hisstop::clock;
use hisstop::detail::{display_path, fmt_clock, is_problem_line, tail, Tail};
use hisstop::sacct::fmt_mem;
use hisstop::text::{clip, fmt_dur, pad, state_glyph};

use crate::model::{cpu_pct, cpu_secs, maxrss, mem_pct, state_colour};
use crate::monitor::Monitor;
use crate::slurm::QueueJob;
use crate::text::{coloured_table, efficiency_table, errors, header_lines, hist_rows, queue_rows, pct, HIST_HEADERS, QUEUE_HEADERS};
use crate::VERSION;

pub const PAD: u16 = 2;
const NOTICE_SECS: f64 = 20.0;
const TAIL_BYTES: u64 = 256 * 1024;

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
    pub tail_scroll: usize,
    pub tail_room: usize,
    pub tail_len: usize,
    pub log_count: usize,
}

#[derive(Debug)]
pub struct Ui {
    pub cursor: usize,
    pub scroll: usize,
    /// the cursor moves over finished jobs instead of the queue
    pub focus_hist: bool,
    pub filter: String,
    pub filter_edit: bool,
    pub sort: Sort,
    pub show_help: bool,
    pub show_eff: bool,
    pub detail: Option<Detail>,
    pub confirm: Option<(String, String)>,
    pub allow_cancel: bool,
    pub notices: Vec<Notice>,
    pub list_len: usize,
    pub list_room: usize,
    requests: Vec<Request>,
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

fn matches(hay: &str, needle: &str) -> bool {
    needle.is_empty() || hay.to_lowercase().contains(&needle.to_lowercase())
}

impl Ui {
    pub fn new() -> Ui {
        Ui {
            cursor: 0,
            scroll: 0,
            focus_hist: false,
            filter: String::new(),
            filter_edit: false,
            sort: Sort::State,
            show_help: false,
            show_eff: false,
            detail: None,
            confirm: None,
            allow_cancel: true,
            notices: Vec::new(),
            list_len: 0,
            list_room: 1,
            requests: Vec::new(),
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

    /// Queue jobs after the filter and sort.
    pub fn visible_jobs(&self, mon: &Monitor) -> Vec<QueueJob> {
        let mut jobs = mon.jobs();
        jobs.retain(|j| [j.jobid.as_str(), j.name.as_str(), j.partition.as_str(), j.state.as_str(), j.nodelist.as_str(), j.reason.as_str()].iter().any(|f| matches(f, &self.filter)));
        let key_f = |v: Option<f64>| -> f64 { v.unwrap_or(-1.0) };
        match self.sort {
            Sort::State => {}
            Sort::Time => jobs.sort_by(|a, b| key_f(b.time_used.map(|t| t as f64)).partial_cmp(&key_f(a.time_used.map(|t| t as f64))).unwrap()),
            Sort::Cpu => jobs.sort_by(|a, b| {
                let ca = key_f(cpu_pct(a, mon.stat_for(&a.jobid).as_ref(), mon.hist_for(&a.jobid).as_ref()));
                let cb = key_f(cpu_pct(b, mon.stat_for(&b.jobid).as_ref(), mon.hist_for(&b.jobid).as_ref()));
                cb.partial_cmp(&ca).unwrap()
            }),
            Sort::Mem => jobs.sort_by(|a, b| {
                let ma = key_f(mem_pct(a, mon.stat_for(&a.jobid).as_ref(), mon.hist_for(&a.jobid).as_ref()));
                let mb = key_f(mem_pct(b, mon.stat_for(&b.jobid).as_ref(), mon.hist_for(&b.jobid).as_ref()));
                mb.partial_cmp(&ma).unwrap()
            }),
            Sort::Name => jobs.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| hisstop::parser::jobkey(&a.jobid).cmp(&hisstop::parser::jobkey(&b.jobid)))),
            Sort::Priority => jobs.sort_by(|a, b| b.priority.cmp(&a.priority)),
        }
        jobs
    }

    pub fn visible_hist(&self, mon: &Monitor) -> Vec<crate::slurm::HistJob> {
        let mut h = mon.history();
        h.retain(|h| [h.row.jobid.as_str(), h.name.as_str(), h.row.state.as_str(), h.partition.as_str()].iter().any(|f| matches(f, &self.filter)));
        h
    }

    /// The job id under the cursor, in whichever list has focus.
    fn cursor_job(&self, mon: &Monitor) -> Option<(String, String)> {
        if self.focus_hist {
            let h = self.visible_hist(mon);
            h.get(self.cursor.min(h.len().saturating_sub(1))).filter(|_| !h.is_empty()).map(|h| (h.row.jobid.clone(), h.name.clone()))
        } else {
            let j = self.visible_jobs(mon);
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
        if !self.focus_hist && focused && self.sort != Sort::State {
            s.push_str(&format!("   by {}", self.sort.label()));
        }
        if focused && self.focus_hist {
            s.push_str("   f back to queue");
        }
        s
    }

    // ---- drawing

    pub fn draw(&mut self, mon: &Monitor, buf: &mut Buffer) {
        let (h, w) = (buf.area.height as i32, buf.area.width as i32);
        let x = PAD as i32;
        if h < 10 || w < 60 {
            put(buf, 0, 0, "terminal too small (need 60x10)", style(Colour::Plain));
            return;
        }
        let (l1, l2) = header_lines(mon, (w - x) as i64);
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
                put(buf, y, xx, &format!("{}{} {}", sep, if *hisstop::text::UNICODE { "\u{25b6}" } else { ">" }, n.text), style(colour_named(n.colour)).add_modifier(Modifier::BOLD));
            }
            y += 1;
        }
        y += 1;
        if self.show_help {
            self.draw_help(buf, y);
            return;
        }
        if self.show_eff {
            self.draw_efficiency(mon, buf, y, w, h);
            return;
        }

        let all_jobs = mon.jobs();
        let jobs = self.visible_jobs(mon);
        let all_hist = mon.history();
        let hist = self.visible_hist(mon);
        let footer_y = h - 1;
        let hist_h = if all_hist.is_empty() { 0 } else { ((hist.len() as i32 + 3).min(((footer_y - y) * 2 / 5).max(5))).min(footer_y - y - 6) };
        let hist_h = if self.focus_hist { (footer_y - y - 6).max(hist_h) } else { hist_h.max(0) };
        let queue_h = footer_y - y - hist_h;

        let (lines, colours) = coloured_table(queue_rows(mon, &jobs), QUEUE_HEADERS, (w - 2 * x) as i64);
        let hint = self.list_hint(jobs.len(), all_jobs.len(), !self.focus_hist);
        self.draw_list(buf, y, queue_h, w, "QUEUE", &hint, &lines, &colours, !self.focus_hist, &all_jobs.is_empty().then(|| if mon.shared().jobs_time.is_none() { "waiting for squeue".to_string() } else { format!("no jobs in the queue for {}", mon.user) }));
        if hist_h > 0 {
            let (lines, colours) = coloured_table(hist_rows(&hist), HIST_HEADERS, (w - 2 * x) as i64);
            let hint = format!("{}   f {}", self.list_hint(hist.len(), all_hist.len(), self.focus_hist), if self.focus_hist { "" } else { "to move through them" });
            self.draw_list(buf, y + queue_h, hist_h, w, "FINISHED", hint.trim_end(), &lines, &colours, self.focus_hist, &None);
        }

        if let Some((id, name)) = &self.confirm {
            put(buf, footer_y, x, &format!("scancel {} ({})?   y confirms, any other key keeps it", id, name), style(Colour::Failed).add_modifier(Modifier::BOLD));
        } else if self.filter_edit {
            put(buf, footer_y, x, &format!("filter: {}_", self.filter), style(Colour::Pending).add_modifier(Modifier::BOLD));
            put(buf, footer_y, x + 10 + self.filter.chars().count() as i32, "   enter keep   esc clear", style(Colour::Dim));
        } else {
            let mut keys = vec!["q quit", "j/k move", "enter job", "/ filter", "s sort", "f finished", "e efficiency", "r refresh", "? help"];
            if self.allow_cancel {
                keys.insert(3, "x scancel");
            }
            put(buf, footer_y, x, &keys.join("   "), style(Colour::Dim));
        }
        if self.detail.is_some() {
            self.draw_detail(mon, buf);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_list(&mut self, buf: &mut Buffer, y: i32, hgt: i32, w: i32, title: &str, hint: &str, lines: &[String], colours: &[&'static str], focused: bool, empty_msg: &Option<String>) {
        if hgt < 3 {
            return;
        }
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
        for (i, line) in body.iter().skip(scroll).take(room).enumerate() {
            let mut st = style(colour_named(colours.get(scroll + i).copied().unwrap_or("dim")));
            if focused && scroll + i == self.cursor {
                st = st.add_modifier(Modifier::REVERSED | Modifier::BOLD);
            }
            put(buf, y + 2 + i as i32, x, line, st);
        }
        if body.is_empty() {
            put(buf, y + 2, x, empty_msg.as_deref().unwrap_or("none"), style(Colour::Dim));
        }
        if body.len() > room && room > 0 {
            put(buf, y + 1 + room as i32, w - 18, &format!("{}-{} of {}", scroll + 1, body.len().min(scroll + room), body.len()), style(Colour::Dim));
        }
    }

    fn draw_efficiency(&self, mon: &Monitor, buf: &mut Buffer, y: i32, w: i32, h: i32) {
        let x = PAD as i32;
        let (lines, colours) = efficiency_table(mon, (w - 2 * x) as i64);
        let n = lines.len().saturating_sub(1);
        self.section(buf, y, "EFFICIENCY", &format!("finished jobs grouped by name, {}   e back", mon.opts.since.replace("now-", "last ")));
        let mut yy = y + 2;
        if n == 0 {
            let sh = mon.shared();
            let why = if !sh.use_sacct { "sacct is off or missing" } else if !sh.hist_err.is_empty() { sh.hist_err.as_str() } else if sh.hist_time.is_none() { "waiting for sacct" } else { "no finished job in the window; widen it with --since" };
            put(buf, yy, x, &format!("nothing to show: {}", why), style(Colour::Pending));
        } else {
            for (i, line) in lines.iter().enumerate() {
                if yy >= h - 4 {
                    break;
                }
                let st = if i == 0 { style(Colour::Dim) } else { style(colour_named(colours[i - 1])) };
                put(buf, yy, x, line, st);
                yy += 1;
            }
            yy += 1;
            put(buf, yy, x, "time = max Elapsed / Timelimit   mem = max MaxRSS / ReqMem   cpu = median TotalCPU / (Elapsed x AllocCPUS)", style(Colour::Dim));
            put(buf, yy + 1, x, "red: OOM / TIMEOUT / NODE_FAIL / failed   yellow: >90% of a limit, or memory over-requested", style(Colour::Dim));
        }
        put(buf, h - 1, x, "q quit   e back   ? help", style(Colour::Dim));
    }

    fn draw_help(&self, buf: &mut Buffer, y: i32) {
        let lines = [
            format!("queuetop {}", VERSION),
            String::new(),
            "  j / k, arrows  move the cursor (J / K, PgUp / PgDn by page)".to_string(),
            "  Enter          open the job: scontrol details, live CPU and memory, sacct figures, output file tail".to_string(),
            "                 (in the popup: j/k scroll the file, G follow its end, l stdout/stderr, Esc close)".to_string(),
            "  f              move through the finished jobs instead of the queue / back".to_string(),
            "  /              filter by jobid, name, partition, state, node or reason (Esc clears)".to_string(),
            "  s              sort the queue: state, time, cpu%, mem%, name, priority".to_string(),
            "  e              efficiency of finished jobs grouped by name / back".to_string(),
            "  x              scancel the job under the cursor (or in the popup), after y/n".to_string(),
            "  r              poll everything now".to_string(),
            "  ?              close this help   q quit".to_string(),
            String::new(),
            "cpu% = CPU seconds used / (elapsed x cores), from sstat while running, sacct afterwards.".to_string(),
            "mem% = MaxRSS / memory requested. Colours: green running, yellow pending, red ended badly, dim completed.".to_string(),
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
    fn detail_rows(&self, mon: &Monitor, jobid: &str) -> (Vec<(String, String, &'static str)>, String) {
        let now = clock::now();
        let job = mon.jobs().into_iter().find(|j| j.jobid == jobid);
        let hist = mon.hist_for(jobid);
        let stat = mon.stat_for(jobid);
        let details = mon.details_for(jobid);
        let mut rows: Vec<(String, String, &'static str)> = Vec::new();
        let mut push = |k: &str, v: String, c: &'static str| {
            if !v.is_empty() {
                rows.push((k.to_string(), v, c));
            }
        };
        let name = job.as_ref().map(|j| j.name.clone()).or_else(|| hist.as_ref().map(|h| h.name.clone())).unwrap_or_default();
        if let Some(j) = &job {
            let mut st = j.state.clone();
            if j.is_pending() && !j.reason.is_empty() && j.reason != "None" {
                st.push_str(&format!(" ({})", j.reason));
            }
            if let Some(t) = j.start.filter(|_| j.is_pending()) {
                if t > now {
                    st.push_str(&format!(" \u{b7} expected to start in {}", fmt_dur(Some(t - now))));
                }
            }
            push("state", st, state_colour(&j.state));
            push("where", format!("partition {} \u{b7} account {} \u{b7} qos {}{}", j.partition, j.account, j.qos, if j.nodelist.is_empty() { String::new() } else { format!(" \u{b7} node {}", j.nodelist) }), "plain");
            let mut res = format!("{} cpu{} \u{b7} {} nodes", j.cpus, if j.cpus == 1 { "" } else { "s" }, j.nodes);
            if let Some(m) = j.mem_req {
                res.push_str(&format!(" \u{b7} {} memory", fmt_mem(m)));
            }
            push("request", res, "plain");
            let mut t = format!("{} of {}", fmt_dur(j.time_used.map(|t| t as f64)), fmt_dur(j.time_limit.map(|t| t as f64)));
            if let Some(u) = j.time_use() {
                t.push_str(&format!(" ({:.0}%)", u * 100.0));
            }
            if let Some(d) = j.deadline().filter(|_| j.is_running()) {
                t.push_str(&format!(" \u{b7} limit reached at {}", fmt_clock(d)));
            }
            push("time", t, if j.time_use().is_some_and(|u| u > 0.9) { "pending" } else { "plain" });
            if j.is_running() {
                let cpu = cpu_pct(j, stat.as_ref(), hist.as_ref());
                let rss = maxrss(stat.as_ref(), hist.as_ref());
                let memp = mem_pct(j, stat.as_ref(), hist.as_ref());
                let mut u = String::new();
                if let Some(c) = cpu_secs(j, stat.as_ref(), hist.as_ref()) {
                    u.push_str(&format!("cpu {} busy ({} CPU-seconds)", pct(cpu), c as i64));
                }
                if let Some(r) = rss {
                    if !u.is_empty() {
                        u.push_str(" \u{b7} ");
                    }
                    u.push_str(&format!("MaxRSS {}", fmt_mem(r)));
                    if let Some(m) = memp {
                        u.push_str(&format!(" ({:.0}% of request)", m * 100.0));
                    }
                }
                if u.is_empty() {
                    u = "no usage data yet (sstat answers once the first step runs)".to_string();
                }
                let colour = if memp.is_some_and(|m| m > 0.9) { "failed" } else if cpu.is_some_and(|c| c < 0.25 && j.time_used.unwrap_or(0) > 300) { "pending" } else { "plain" };
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
            push("when", when.join(" \u{b7} "), "plain");
            if !j.dependency.is_empty() && j.dependency != "(null)" {
                push("depends", j.dependency.clone(), "pending");
            }
            if !j.array_task.is_empty() && j.array_task != "N/A" {
                push("array", format!("job {} task {}", j.array_job, j.array_task), "plain");
            }
            push("priority", j.priority.to_string(), "plain");
            push("workdir", j.workdir.clone(), "plain");
        }
        if let Some(h) = &hist {
            if h.row.is_final() {
                push("sacct", h.row.summary(), if h.row.is_bad() { "failed" } else if h.row.near_limit() { "pending" } else { "plain" });
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
                push("when", when.join(" \u{b7} "), "plain");
                push("where", format!("partition {}{}", h.partition, if h.row.nodelist.is_empty() { String::new() } else { format!(" \u{b7} node {}", h.row.nodelist) }), "plain");
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

    fn draw_detail(&mut self, mon: &Monitor, buf: &mut Buffer) {
        let Some(mut d) = self.detail.take() else { return };
        let area = buf.area;
        let pw = (area.width as u32 * 9 / 10).clamp(48, area.width as u32) as u16;
        let ph = (area.height as u32 * 9 / 10).clamp(10, area.height as u32) as u16;
        let rect = Rect::new((area.width - pw) / 2, (area.height - ph) / 2, pw, ph);
        Clear.render(rect, buf);
        let (rows, name) = self.detail_rows(mon, &d.jobid);
        let state_colour_name = rows.first().map(|(_, _, c)| *c).unwrap_or("plain");
        let tc = colour_named(state_colour_name);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(style(tc))
            .title(format!(" job {} \u{b7} {} ", d.jobid, clip(&name, 50)))
            .title_style(style(tc).add_modifier(Modifier::BOLD))
            .title_bottom(" Esc close   j/k scroll   G follow   l stdout/stderr   x scancel ");
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
        let job = mon.jobs().into_iter().find(|j| j.jobid == d.jobid);
        let details = mon.details_for(&d.jobid);
        let (logs, missing) = Self::log_candidates(details.as_ref(), job.as_ref());
        d.log_count = logs.len();
        let details_h = (out.len() as i32).min((ih * 55 / 100).max(3));
        for (i, (line, st)) in out.iter().enumerate().take(details_h as usize) {
            put(buf, iy + i as i32, ix, &pad(line, iw as i64), *st);
        }
        if out.len() as i32 > details_h && details_h > 0 {
            put(buf, iy + details_h - 1, ix + iw as i32 - 10, &format!("+{} more", out.len() as i32 - details_h), style(Colour::Dim));
        }
        let ly = iy + details_h;
        if logs.is_empty() {
            put(buf, ly, ix, "OUTPUT", style(Colour::Title));
            put(buf, ly, ix + 8, if details.is_some() { "no output file yet" } else { "waiting for scontrol" }, style(Colour::Dim));
            for (i, m) in missing.iter().enumerate() {
                put(buf, ly + 1 + i as i32, ix + 2, &m.display().to_string(), style(Colour::Dim));
            }
            d.tail_len = 0;
            self.detail = Some(d);
            return;
        }
        d.log_idx %= logs.len();
        let path = &logs[d.log_idx];
        let room = (ih - details_h - 1).max(1) as usize;
        d.tail_room = room;
        let wd = job.as_ref().map(|j| PathBuf::from(&j.workdir)).unwrap_or_default();
        let mut head = format!("OUTPUT  {}", display_path(&wd, path));
        if logs.len() > 1 {
            head.push_str(&format!("   [{}/{}, l for next]", d.log_idx + 1, logs.len()));
        }
        match tail(path, TAIL_BYTES) {
            Ok(Tail { lines, size, age, .. }) => {
                let t = Tail { lines: Vec::new(), size, age, truncated: false };
                head.push_str(&format!("   {}", t.describe()));
                d.tail_len = lines.len();
                d.tail_scroll = d.tail_scroll.min(lines.len().saturating_sub(room));
                if d.tail_scroll > 0 {
                    head.push_str(&format!("   \u{2191} {} lines up, G to follow", d.tail_scroll));
                }
                put(buf, ly, ix, &clip(&head, iw as i64), style(Colour::Title));
                let end = lines.len() - d.tail_scroll;
                let start = end.saturating_sub(room);
                if lines.is_empty() {
                    put(buf, ly + 1, ix, "(empty)", style(Colour::Dim));
                }
                for (i, line) in lines[start..end].iter().enumerate() {
                    let st = if is_problem_line(line) { style(Colour::Failed) } else { style(Colour::Plain) };
                    put(buf, ly + 1 + i as i32, ix, &pad(&line.replace('\t', "    "), iw as i64), st);
                }
            }
            Err(e) => {
                put(buf, ly, ix, &clip(&head, iw as i64), style(Colour::Title));
                put(buf, ly + 1, ix, &format!("cannot read: {}", e), style(Colour::Failed));
                d.tail_len = 0;
            }
        }
        self.detail = Some(d);
    }

    // ---- keys

    fn ask_cancel(&mut self, mon: &Monitor, jobid: &str, name: &str) {
        if !self.allow_cancel {
            self.notify("scancel is off (--no-cancel)", "pending");
            return;
        }
        if mon.jobs().iter().any(|j| j.jobid == jobid) {
            self.confirm = Some((jobid.to_string(), name.to_string()));
        } else {
            self.notify(format!("job {} is not in the queue any more", jobid), "pending");
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
                    if d.log_count > 0 {
                        d.log_idx = (d.log_idx + 1) % d.log_count;
                        d.tail_scroll = 0;
                    }
                }
                Key::Char('x') | Key::Char('X') => {
                    let id = d.jobid.clone();
                    let name = mon.jobs().into_iter().find(|j| j.jobid == id).map(|j| j.name).unwrap_or_default();
                    self.ask_cancel(mon, &id, &name);
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
                self.sort = self.sort.next();
                self.cursor = 0;
            }
            Key::Char('f') => {
                self.focus_hist = !self.focus_hist;
                self.cursor = 0;
                self.scroll = 0;
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
                if let Some((id, _)) = self.cursor_job(mon) {
                    mon.request_details(&id);
                    self.detail = Some(Detail { jobid: id, log_idx: 0, tail_scroll: 0, tail_room: 1, tail_len: 0, log_count: 0 });
                }
            }
            Key::Char('x') | Key::Char('X') => {
                if let Some((id, name)) = self.cursor_job(mon) {
                    self.ask_cancel(mon, &id, &name);
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

/// Glyph + state for the popup title area and tables that want it.
pub fn state_with_glyph(state: &str) -> String {
    format!("{} {}", state_glyph(state_colour(state)), state)
}
