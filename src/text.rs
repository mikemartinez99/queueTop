//! Formatting helpers, the tables, and the plain-text snapshot (`--once`).

use crate::clock;
use crate::model::{cpu_pct, efficiency_by_name, efficiency_rows, maxrss, mem_pct, summary};
use crate::monitor::{Monitor, Snap};
use crate::slurm::{HistJob, QueueJob};

// ------------------------------------------------------------------ formatting

pub fn fmt_dur(seconds: Option<f64>) -> String {
    let s = match seconds {
        Some(s) if s >= 0.0 => s as i64,
        _ => return "-".to_string(),
    };
    let (h, s) = (s / 3600, s % 3600);
    let (m, s) = (s / 60, s % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

pub fn fmt_age(seconds: Option<f64>) -> String {
    match seconds {
        None => "-".to_string(),
        Some(s) if s < 90.0 => format!("{}s", s as i64),
        Some(s) if s < 5400.0 => format!("{}m", (s / 60.0).floor() as i64),
        Some(s) => format!("{:.1}h", s / 3600.0),
    }
}

pub fn fmt_mem(bytes: u64) -> String {
    let b = bytes as f64;
    const G: f64 = 1024.0 * 1024.0 * 1024.0;
    const M: f64 = 1024.0 * 1024.0;
    if b >= G {
        format!("{:.1}G", b / G)
    } else if b >= M {
        format!("{:.0}M", b / M)
    } else {
        format!("{:.0}K", b / 1024.0)
    }
}

pub fn fmt_clock(epoch: f64) -> String {
    use chrono::{Local, TimeZone};
    Local.timestamp_opt(epoch as i64, 0).single().map(|t| t.format("%b %e %H:%M:%S").to_string()).unwrap_or_else(|| "-".to_string())
}

pub fn pct(v: Option<f64>) -> String {
    v.map(|u| format!("{:.0}%", u * 100.0)).unwrap_or_else(|| "-".to_string())
}

fn mem(v: Option<u64>) -> String {
    v.map(fmt_mem).unwrap_or_else(|| "-".to_string())
}

/// Cut to `width` characters, marking the cut with `~`.
pub fn clip(s: &str, width: i64) -> String {
    if width <= 0 {
        return String::new();
    }
    let n = s.chars().count() as i64;
    if n <= width {
        return s.to_string();
    }
    if width > 1 {
        let mut out: String = s.chars().take((width - 1) as usize).collect();
        out.push('~');
        out
    } else {
        s.chars().take(width as usize).collect()
    }
}

/// Clip or space-pad to exactly `width` characters.
pub fn pad(s: &str, width: i64) -> String {
    let mut out = clip(s, width);
    let n = out.chars().count() as i64;
    if n < width {
        out.extend(std::iter::repeat_n(' ', (width - n) as usize));
    }
    out
}

/// The marker in front of a queue row and the colour it is drawn in:
/// `*` running ("alert", red, once past 90% of the time limit or the memory
/// request), `^` pending, `#` anything stopped.
pub fn queue_glyph(job: &QueueJob, mem_use: Option<f64>) -> (&'static str, &'static str) {
    if job.is_running() {
        let near = job.time_use().is_some_and(|t| t > 0.9) || mem_use.is_some_and(|m| m > 0.9);
        ("*", if near { "alert" } else { "running" })
    } else if job.is_pending() {
        ("^", "pending")
    } else {
        ("#", "failed")
    }
}

/// The marker in front of a finished row and the colour of the whole row:
/// `+` completed, `!` an OOM kill, `x` anything else that ended badly.
pub fn hist_glyph(h: &HistJob) -> (&'static str, &'static str) {
    if h.is_oom() {
        ("!", "failed")
    } else if h.is_bad() {
        ("x", "failed")
    } else {
        ("+", "done")
    }
}

pub fn terminal_width() -> i64 {
    if let Ok(v) = std::env::var("COLUMNS") {
        if let Ok(n) = v.trim().parse::<i64>() {
            if n > 0 {
                return n;
            }
        }
    }
    crossterm::terminal::size().map(|(w, _)| w as i64).unwrap_or(100)
}

// ------------------------------------------------------------------ tables

/// Rows as aligned text lines (header first), clipped to `width`. The last
/// column gets at least `last_min` characters; the others shrink to make room.
pub fn table(rows: &[Vec<String>], headers: &[&str], width: i64, last_min: i64) -> Vec<String> {
    if rows.is_empty() {
        return vec![clip(&headers.join("   "), width)];
    }
    let ncol = headers.len();
    let mut w: Vec<i64> = headers.iter().map(|h| h.chars().count() as i64).collect();
    for r in rows {
        for (i, wi) in w.iter_mut().enumerate() {
            let n = r.get(i).map(|c| c.chars().count()).unwrap_or(0) as i64;
            *wi = (*wi).max(n);
        }
    }
    let gap: i64 = 3;
    let marker = headers[0].is_empty() && ncol > 1;
    let fixed_of = |w: &[i64]| w[..ncol - 1].iter().sum::<i64>() + gap * (ncol as i64 - 1) - if marker { gap - 1 } else { 0 };
    let mut fixed = fixed_of(&w);
    while fixed > width - last_min && w[..ncol - 1].iter().copied().max().unwrap_or(0) > 8 {
        let mut i = 0;
        for k in 0..ncol - 1 {
            if w[k] > w[i] {
                i = k;
            }
        }
        w[i] = ((w[i] as f64 * 0.7) as i64).max(8);
        fixed = fixed_of(&w);
    }
    w[ncol - 1] = (width - fixed).max(4);
    // a marker column (empty header) sits one space from its row
    let sep = " ".repeat(gap as usize);
    let join = |cells: Vec<String>| -> String {
        let mut out = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                out.push_str(if i == 1 && marker { " " } else { sep.as_str() });
            }
            out.push_str(c);
        }
        out
    };
    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(join(headers.iter().enumerate().map(|(i, h)| pad(h, w[i])).collect()));
    for r in rows {
        lines.push(clip(&join((0..ncol).map(|i| pad(r.get(i).map(String::as_str).unwrap_or(""), w[i])).collect()), width));
    }
    lines
}

pub const QUEUE_HEADERS: &[&str] = &["", "jobid", "name", "partition", "state", "time", "limit", "cpus", "cpu%", "mem req", "maxrss", "mem%", "node / reason"];
pub const HIST_HEADERS: &[&str] = &["", "jobid", "name", "state", "elapsed", "limit", "maxrss", "req", "mem%", "cpu%", "ended"];
pub const EFF_HEADERS: &[&str] = &["name", "jobs", "elapsed max", "limit", "time", "maxrss", "reqmem", "mem", "cpu", "next request", "note"];

/// Live cpu%, mem% and MaxRSS of a running job, or the worst over the tasks of a collapsed array.
pub fn live_use(snap: &Snap, j: &QueueJob) -> (Option<f64>, Option<f64>, Option<u64>) {
    if j.is_group() {
        let max_f = |a: Option<f64>, b: Option<f64>| b.map(|b| a.map_or(b, |a| a.max(b))).or(a);
        let mut out: (Option<f64>, Option<f64>, Option<u64>) = (None, None, None);
        for t in &j.tasks {
            let (c, m, r) = live_use(snap, t);
            out = (max_f(out.0, c), max_f(out.1, m), out.2.max(r));
        }
        return out;
    }
    if !j.is_running() {
        return (None, None, None);
    }
    let (stat, hist) = (snap.stat_for(&j.jobid), snap.hist_for(&j.jobid));
    (cpu_pct(j, stat, hist), mem_pct(j, stat, hist), maxrss(stat, hist))
}

/// One row per queue job, glyph first, with the colour name of its glyph.
pub fn queue_rows(snap: &Snap, jobs: &[QueueJob]) -> Vec<(Vec<String>, &'static str)> {
    jobs.iter()
        .map(|j| {
            let (cpu, memp, rss) = live_use(snap, j);
            let where_ = if j.is_group() {
                j.reason.clone()
            } else if j.is_running() {
                j.nodelist.clone()
            } else if j.reason.is_empty() || j.reason == "None" {
                "-".to_string()
            } else {
                format!("({})", j.reason)
            };
            let (glyph, colour) = queue_glyph(j, memp);
            let row = vec![
                glyph.to_string(),
                j.jobid.clone(),
                j.name.clone(),
                j.partition.clone(),
                j.state.clone(),
                fmt_dur(j.time_used.map(|t| t as f64)),
                fmt_dur(j.time_limit.map(|t| t as f64)),
                j.cpus.to_string(),
                pct(cpu),
                mem(j.mem_req),
                mem(rss),
                pct(memp),
                where_,
            ];
            (row, colour)
        })
        .collect()
}

/// One row per finished job, glyph first, with the colour name of the whole row.
pub fn hist_rows(hist: &[HistJob]) -> Vec<(Vec<String>, &'static str)> {
    hist.iter()
        .map(|h| {
            let (glyph, colour) = hist_glyph(h);
            let row = vec![
                glyph.to_string(),
                h.jobid.clone(),
                h.name.clone(),
                h.short(),
                fmt_dur(h.elapsed.map(|t| t as f64)),
                fmt_dur(h.timelimit.map(|t| t as f64)),
                mem(h.max_rss),
                mem(h.req_mem),
                pct(h.mem_use()),
                pct(h.cpu_eff()),
                h.end.map(fmt_clock).unwrap_or_else(|| "-".to_string()),
            ];
            (row, colour)
        })
        .collect()
}

/// Lines (header first) and per-row colour names.
pub fn coloured_table(rows: Vec<(Vec<String>, &'static str)>, headers: &[&str], width: i64) -> (Vec<String>, Vec<&'static str>) {
    let colours: Vec<&'static str> = rows.iter().map(|(_, c)| *c).collect();
    let plain: Vec<Vec<String>> = rows.into_iter().map(|(r, _)| r).collect();
    (table(&plain, headers, width, 16), colours)
}

pub fn efficiency_table(hist: &[HistJob], width: i64) -> (Vec<String>, Vec<&'static str>) {
    let effs = efficiency_by_name(hist);
    let colours: Vec<&'static str> = effs.iter().map(|e| e.note().1).collect();
    (table(&efficiency_rows(&effs), EFF_HEADERS, width, 20), colours)
}

// ------------------------------------------------------------------ header and snapshot

/// Two header lines: who and what; then freshness of each source.
pub fn header_lines(mon: &Monitor, snap: &Snap, width: i64) -> (String, String) {
    let now = clock::now();
    let s = summary(&snap.jobs, &snap.hist);
    let mut bits = vec!["queuetop".to_string(), format!("{}@{}", mon.user, mon.cluster), format!("{} running", s.running), format!("{} pending", s.pending)];
    if s.other > 0 {
        bits.push(format!("{} other", s.other));
    }
    if s.running > 0 {
        bits.push(format!("{} cpus, {} in use", s.cpus_running, fmt_mem(s.mem_running)));
    }
    if let Some(t) = s.next_start {
        if t > now {
            bits.push(format!("next start in {}", fmt_dur(Some(t - now))));
        }
    }
    if s.finished > 0 {
        bits.push(format!("{} finished, {} badly", s.finished, s.finished_bad));
    }
    let line1 = clip(&bits.join("   "), width);
    let sh = mon.shared();
    let mut info: Vec<String> = Vec::new();
    let age = |t: Option<f64>| t.map(|t| fmt_age(Some(now - t))).unwrap_or_else(|| "never".to_string());
    info.push(format!("squeue {} ago", age(sh.jobs_time)));
    if sh.use_sstat {
        info.push(format!("sstat {} ago", age(sh.stats_time)));
    }
    if sh.use_sacct {
        info.push(format!("sacct {} ago ({})", age(sh.hist_time), mon.opts.since.replace("now-", "last ")));
    }
    (line1, clip(&info.join(" | "), width))
}

pub fn errors(mon: &Monitor) -> Vec<String> {
    let s = mon.shared();
    [s.jobs_err.clone(), s.stats_err.clone(), s.hist_err.clone()].into_iter().filter(|e| !e.is_empty()).collect()
}

pub fn snapshot_text(mon: &Monitor, width: Option<i64>) -> String {
    let width = width.unwrap_or_else(terminal_width);
    let snap = mon.snapshot();
    let (l1, l2) = header_lines(mon, &snap, width);
    let mut out = vec![l1, l2];
    for e in errors(mon) {
        out.push(format!("  {}", e));
    }
    out.push(String::new());
    out.push(format!("Queue ({}):", snap.jobs.len()));
    out.extend(coloured_table(queue_rows(&snap, &snap.jobs), QUEUE_HEADERS, width).0);
    if !snap.hist.is_empty() {
        out.push(String::new());
        out.push(format!("Finished ({}):", snap.hist.len()));
        out.extend(coloured_table(hist_rows(&snap.hist), HIST_HEADERS, width).0);
        out.push(String::new());
        out.push("Efficiency by job name:".to_string());
        out.extend(efficiency_table(&snap.hist, width).0);
    }
    out.join("\n")
}
