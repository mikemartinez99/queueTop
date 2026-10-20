//! Tables and the plain-text snapshot (`--once`).

use hisstop::clock;
use hisstop::efficiency::efficiency_rows;
use hisstop::sacct::fmt_mem;
use hisstop::text::{clip, fmt_age, fmt_dur, table, table_with_last, UNICODE};

use crate::model::{cpu_pct, efficiency_by_name, maxrss, mem_pct, state_colour, summary};
use crate::monitor::Monitor;
use crate::slurm::{HistJob, QueueJob};

pub const QUEUE_HEADERS: &[&str] = &["jobid", "name", "partition", "state", "time", "limit", "cpus", "cpu%", "mem req", "maxrss", "mem%", "node / reason"];
pub const HIST_HEADERS: &[&str] = &["jobid", "name", "state", "elapsed", "limit", "maxrss", "req", "mem%", "cpu%", "ended"];
pub const EFF_HEADERS: &[&str] = &["name", "jobs", "elapsed max", "limit", "time", "maxrss", "reqmem", "mem", "cpu", "note"];

pub fn pct(v: Option<f64>) -> String {
    v.map(|u| format!("{:.0}%", u * 100.0)).unwrap_or_else(|| "-".to_string())
}

fn mem(v: Option<u64>) -> String {
    v.map(fmt_mem).unwrap_or_else(|| "-".to_string())
}

/// One row per queue job; the colour name of each row comes with it.
pub fn queue_rows(mon: &Monitor, jobs: &[QueueJob]) -> Vec<(Vec<String>, &'static str)> {
    jobs.iter()
        .map(|j| {
            let stat = mon.stat_for(&j.jobid);
            let hist = mon.hist_for(&j.jobid);
            let (cpu, memp, rss) = if j.is_running() {
                (cpu_pct(j, stat.as_ref(), hist.as_ref()), mem_pct(j, stat.as_ref(), hist.as_ref()), maxrss(stat.as_ref(), hist.as_ref()))
            } else {
                (None, None, None)
            };
            let where_ = if j.is_running() {
                j.nodelist.clone()
            } else if j.reason.is_empty() || j.reason == "None" {
                "-".to_string()
            } else {
                format!("({})", j.reason)
            };
            let row = vec![
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
            (row, state_colour(&j.state))
        })
        .collect()
}

pub fn hist_rows(hist: &[HistJob]) -> Vec<(Vec<String>, &'static str)> {
    hist.iter()
        .map(|h| {
            let r = &h.row;
            let row = vec![
                r.jobid.clone(),
                h.name.clone(),
                r.short(),
                fmt_dur(r.elapsed.map(|t| t as f64)),
                fmt_dur(r.timelimit.map(|t| t as f64)),
                mem(r.max_rss),
                mem(r.req_mem),
                pct(r.mem_use()),
                pct(r.cpu_eff()),
                h.end.map(hisstop::detail::fmt_clock).unwrap_or_else(|| "-".to_string()),
            ];
            (row, state_colour(&r.state))
        })
        .collect()
}

/// Lines (header first) and per-row colours.
pub fn coloured_table(rows: Vec<(Vec<String>, &'static str)>, headers: &[&str], width: i64) -> (Vec<String>, Vec<&'static str>) {
    let colours: Vec<&'static str> = rows.iter().map(|(_, c)| *c).collect();
    let plain: Vec<Vec<String>> = rows.into_iter().map(|(r, _)| r).collect();
    (table_with_last(&plain, headers, width, 16), colours)
}

pub fn efficiency_table(mon: &Monitor, width: i64) -> (Vec<String>, Vec<&'static str>) {
    let effs = efficiency_by_name(&mon.history());
    let colours: Vec<&'static str> = effs.iter().map(|e| e.note().1).collect();
    (table(&efficiency_rows(&effs), EFF_HEADERS, width), colours)
}

/// Two header lines: who and what; then freshness of each source.
pub fn header_lines(mon: &Monitor, width: i64) -> (String, String) {
    let now = clock::now();
    let jobs = mon.jobs();
    let hist = mon.history();
    let s = summary(&jobs, &hist);
    let mut bits = vec![
        "queuetop".to_string(),
        format!("{}@{}", mon.user, mon.cluster),
        format!("{} running", s.running),
        format!("{} pending", s.pending),
    ];
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
    let sep = if *UNICODE { " \u{b7} " } else { " - " };
    (line1, clip(&info.join(sep), width))
}

pub fn errors(mon: &Monitor) -> Vec<String> {
    let s = mon.shared();
    [s.jobs_err.clone(), s.stats_err.clone(), s.hist_err.clone()].into_iter().filter(|e| !e.is_empty()).collect()
}

pub fn snapshot_text(mon: &Monitor, width: Option<i64>) -> String {
    let width = width.unwrap_or_else(hisstop::text::terminal_width);
    let (l1, l2) = header_lines(mon, width);
    let mut out = vec![l1, l2];
    for e in errors(mon) {
        out.push(format!("  {}", e));
    }
    let jobs = mon.jobs();
    out.push(String::new());
    out.push(format!("Queue ({}):", jobs.len()));
    out.extend(coloured_table(queue_rows(mon, &jobs), QUEUE_HEADERS, width).0);
    let hist = mon.history();
    if !hist.is_empty() {
        out.push(String::new());
        out.push(format!("Finished ({}):", hist.len()));
        out.extend(coloured_table(hist_rows(&hist), HIST_HEADERS, width).0);
        out.push(String::new());
        out.push("Efficiency by job name:".to_string());
        out.extend(efficiency_table(mon, width).0);
    }
    out.join("\n")
}
