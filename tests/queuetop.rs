//! queuetop tests: parsers, the monitor against the fake Slurm tools, the
//! screens, and the --once / --json binaries.

use std::fs;
use std::path::{Path, PathBuf};

use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use queuetop::app::App;
use queuetop::events::Hooks;
use queuetop::config::config_args;
use queuetop::events::{describe_all, Event};
use queuetop::model::{cpu_pct, efficiency_by_name, group_arrays, mem_pct, suggest_mem, suggest_time, summary};
use queuetop::slurm::{array_task_count, QueueJob};
use queuetop::monitor::{Monitor, Options};
use queuetop::slurm::{parse_sacct_hist, parse_scontrol, parse_slurm_time, parse_squeue, parse_sstat, since_seconds, Tools};
use queuetop::ui::{HistSort, Mouse};
use queuetop::text::snapshot_text;
use queuetop::ui::{Key, Sort};

fn fakes() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fake");
    use std::os::unix::fs::PermissionsExt;
    for t in ["squeue", "sstat", "sacct", "scontrol", "scancel"] {
        let _ = fs::set_permissions(d.join(t), fs::Permissions::from_mode(0o755));
    }
    d
}

fn run_fake(name: &str) -> String {
    // the popup test sets QUEUETOP_FAKE_STDOUT for the whole process; keep it out of this one
    let out = std::process::Command::new(fakes().join(name)).env_remove("QUEUETOP_FAKE_STDOUT").env_remove("QUEUETOP_FAKE_STDERR").arg("x").arg("y").arg("777").output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The fakes are dated 2026-10-20; freeze the clock that afternoon so the
/// finished jobs stay inside the 24 h window whenever the tests run.
const NOW: &str = "2026-10-20T17:57:00";

/// A monitor that has polled every fake once.
fn make_monitor() -> Monitor {
    std::env::set_var("QUEUETOP_NOW", format!("{}", parse_slurm_time(NOW).unwrap()));
    let opts = Options { squeue_interval: 0.0, sstat_interval: 0.0, sacct_interval: 0.0, ..Default::default() };
    let mut mon = Monitor::new("mike", "discovery", Tools::from_dir(&fakes()), opts);
    mon.wait_all(Some(10.0));
    mon
}

fn render(app: &mut App, w: u16, h: u16) -> Terminal<TestBackend> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.draw(f.buffer_mut())).unwrap();
    term
}

fn screen_lines(term: &Terminal<TestBackend>) -> Vec<String> {
    let buf = term.backend().buffer();
    (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect()
}

fn find_line<'a>(lines: &'a [String], needle: &str) -> Option<(usize, &'a String)> {
    lines.iter().enumerate().find(|(_, l)| l.contains(needle))
}

// ---------------------------------------------------------------- parsers

#[test]
fn squeue_rows_parse() {
    let jobs = parse_squeue(&run_fake("squeue"));
    assert_eq!(jobs.len(), 5);
    let t = &jobs[0];
    assert_eq!((t.jobid.as_str(), t.name.as_str(), t.state.as_str(), t.partition.as_str()), ("9849151", "trimming.2", "RUNNING", "standard"));
    assert_eq!(t.time_used, Some(51 * 60));
    assert_eq!(t.time_limit, Some(7200));
    assert_eq!(t.cpus, 4);
    assert_eq!(t.mem_req, Some(58 << 30));
    assert_eq!(t.nodelist, "t03");
    assert!(t.is_running() && !t.is_pending());
    assert!((t.time_use().unwrap() - 0.425).abs() < 1e-9);
    assert_eq!(t.deadline(), Some(parse_slurm_time("2026-10-20T17:05:49").unwrap() + 7200.0));
    let p = &jobs[3];
    assert!(p.is_pending());
    assert_eq!(p.reason, "Priority");
    assert_eq!(p.dependency, "afterok:9849151");
    assert_eq!(p.start, parse_slurm_time("2026-10-20T18:30:00"));
    let a = &jobs[4];
    assert_eq!(a.jobid, "9849171_[1-3]");
    assert_eq!(a.array_task, "[1-3]");
    assert!(a.start.is_none()); // N/A
    assert!(parse_slurm_time("Unknown").is_none());
}

#[test]
fn sstat_steps_fold_into_jobs() {
    let st = parse_sstat(&run_fake("sstat"));
    assert_eq!(st.len(), 3);
    let t = &st["9849151"];
    assert!((t.cpu_secs.unwrap() - (2.0 * 3600.0 + 2.0 * 60.0 + 0.001)).abs() < 1e-6);
    assert_eq!(t.max_rss, Some(3145728 * 1024));
    assert_eq!(t.steps, 2);
    let r = &st["9849161"];
    assert!((r.cpu_secs.unwrap() - 38.401).abs() < 1e-6); // the unsampled step's NO_VAL sentinel is ignored
    // TRESUsageInTot's cpu= wins over AveCPU when present
    let tres = parse_sstat("1.batch|00:01.000|100K|100K|4|cpu=00:10:00,energy=0,mem=100K\n1.0|213503982334-14:25:51|100K|100K|1|\n");
    assert_eq!(tres["1"].cpu_secs, Some(600.0));
    assert!(queuetop::slurm::tres_cpu_seconds("energy=0,mem=1K").is_none());
    // a step with only the sentinel leaves cpu unknown, not zero
    let none = parse_sstat("2.batch|213503982334-14:25:51|100K|100K|1|\n");
    assert_eq!(none["2"].cpu_secs, None);
    assert_eq!(r.max_rss, Some(52428800 * 1024));
    assert_eq!(r.steps, 3);
    assert!(queuetop::slurm::sane_cpu_time("213503982334-14:25:51").is_none());
    assert_eq!(queuetop::slurm::sane_cpu_time("1-02:03:04.5"), Some(86400.0 + 7384.5));
}

#[test]
fn absurd_cpu_readings_are_not_shown() {
    use queuetop::slurm::{HistJob, QueueJob, Stat};
    let job = QueueJob { jobid: "1".into(), state: "RUNNING".into(), time_used: Some(600), cpus: 4, ..Default::default() };
    let stat = Stat { jobid: "1".into(), cpu_secs: Some(1.8e16), max_rss: None, steps: 1 };
    assert!(cpu_pct(&job, Some(&stat), None).is_none());
    let ok = Stat { jobid: "1".into(), cpu_secs: Some(1200.0), max_rss: None, steps: 1 };
    assert!((cpu_pct(&job, Some(&ok), None).unwrap() - 0.5).abs() < 1e-9);
    // a running job with no live reading shows nothing, even if sacct has a (stale, zero) TotalCPU
    let mut h = HistJob { total_cpu: Some(0.0), ..Default::default() };
    let unknown = Stat { jobid: "1".into(), cpu_secs: None, max_rss: None, steps: 1 };
    assert!(cpu_pct(&job, Some(&unknown), Some(&h)).is_none());
    assert!(cpu_pct(&job, None, Some(&h)).is_none());
    // a finished job uses sacct's TotalCPU
    let done = QueueJob { state: "COMPLETED".into(), ..job.clone() };
    h.total_cpu = Some(2400.0);
    assert!((cpu_pct(&done, None, Some(&h)).unwrap() - 1.0).abs() < 1e-9);
}

#[test]
fn sacct_history_parses_and_groups() {
    let h = parse_sacct_hist(&run_fake("sacct"));
    assert_eq!(h.len(), 6);
    let oom = &h["9849101"];
    assert_eq!(oom.name, "alignment.7");
    assert_eq!(oom.state, "OUT_OF_MEMORY");
    assert!(oom.is_oom() && oom.is_bad());
    assert_eq!(oom.short(), "OOM kill");
    assert_eq!(oom.max_rss, Some(33554432 * 1024));
    assert_eq!(oom.req_mem, Some(32 << 30));
    assert!(oom.mem_use().unwrap() > 0.99);
    assert_eq!(oom.wait(), Some(300.0));
    assert_eq!(oom.partition, "standard");
    let running = &h["9849151"];
    assert!(!running.is_final());
    assert!(running.end.is_none());
    let effs = efficiency_by_name(&h.values().cloned().collect::<Vec<_>>());
    let names: Vec<&str> = effs.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"trimming.1") && names.contains(&"alignment.7"));
    let al = effs.iter().find(|e| e.name == "alignment.7").unwrap();
    assert_eq!(al.oom, 1);
    assert_eq!(al.note().1, "failed");
    let qc = effs.iter().find(|e| e.name == "qc_report").unwrap();
    assert_eq!(qc.timeout, 1);
}

#[test]
fn scontrol_key_values() {
    let kv = parse_scontrol(&run_fake("scontrol"));
    assert_eq!(kv["JobId"], "777");
    assert_eq!(kv["JobName"], "trimming.2");
    assert_eq!(kv["WorkDir"], "/dartfs/rc/lab/rnaseq");
    assert_eq!(kv["StdOut"], "/dartfs/rc/lab/rnaseq/slurm_logs/log_trimming_777.out");
    assert_eq!(kv["Command"], "/dartfs/rc/lab/rnaseq/.snakemake/tmp.abc/snakejob.trimming.2.sh --cores 4");
    assert_eq!(kv["Comment"], "a comment with spaces");
    assert_eq!(kv["TRES"], "cpu=4,mem=58G,node=1,billing=4"); // '=' inside a value stays inside it
    assert_eq!(kv["AllocNode:Sid"], "login01:12345");
}

// ---------------------------------------------------------------- monitor + model

#[test]
fn monitor_polls_all_sources() {
    let mon = make_monitor();
    let jobs = mon.jobs();
    assert_eq!(jobs.len(), 5);
    assert!(jobs[0].is_running() && jobs[3].is_pending()); // running first
    let snap = mon.snapshot();
    let s = snap.stat_for("9849151").unwrap();
    assert_eq!(s.max_rss, Some(3145728 * 1024));
    let hist = mon.history();
    assert_eq!(hist.len(), 5); // the running one is not history
    assert_eq!(hist[0].jobid, "9849101"); // most recently ended first
    let t = &jobs[0];
    let cpu = cpu_pct(t, snap.stat_for(&t.jobid), None).unwrap();
    assert!((cpu - (7320.001 / (3060.0 * 4.0))).abs() < 1e-6); // ~60%
    let mem = mem_pct(t, snap.stat_for(&t.jobid), None).unwrap();
    assert!((mem - 3.0 / 58.0).abs() < 1e-6);
    let sum = summary(&jobs, &hist);
    assert_eq!((sum.running, sum.pending, sum.other), (3, 2, 0));
    assert_eq!(sum.cpus_running, 16);
    assert_eq!((sum.finished, sum.finished_bad), (5, 3));
    assert_eq!(sum.next_start, parse_slurm_time("2026-10-20T18:30:00"));
    // for a running job sacct's TotalCPU never stands in: without a live reading the cell stays unknown
    let h = snap.hist_for("9849151").unwrap();
    assert!(cpu_pct(t, None, Some(h)).is_none());
    let errs = queuetop::text::errors(&mon);
    assert!(errs.is_empty(), "{:?}", errs);
}

#[test]
fn snapshot_text_sections() {
    let mon = make_monitor();
    let txt = snapshot_text(&mon, Some(160));
    eprintln!("{}", txt);
    assert!(txt.starts_with("queuetop   mike@discovery   3 running   2 pending   16 cpus, 359.0G in use"));
    assert!(txt.contains("5 finished, 3 badly"));
    assert!(txt.contains("Queue (5):"));
    assert!(regex::Regex::new(r"\*\s9849151\s+trimming\.2\s+standard\s+RUNNING\s+51:00\s+2:00:00\s+4\s+60%\s+58\.0G\s+3\.0G\s+5%\s+t03").unwrap().is_match(&txt));
    assert!(regex::Regex::new(r"\^\s9849170\s+alignment\.8\s+standard\s+PENDING\s+0:00\s+2:00:00\s+8\s+-\s+32\.0G\s+-\s+-\s+\(Priority\)").unwrap().is_match(&txt));
    assert!(txt.contains("Finished (5):"));
    assert!(regex::Regex::new(r"!\s9849101\s+alignment\.7\s+OOM kill\s+40:00\s+2:00:00\s+32\.0G\s+32\.0G\s+100%\s+47%").unwrap().is_match(&txt));
    assert!(regex::Regex::new(r"\+\s9849100\s+trimming\.1\s+COMPLETED").unwrap().is_match(&txt));
    assert!(regex::Regex::new(r"x\s9849102\s+qc_report\s+TIMEOUT").unwrap().is_match(&txt));
    assert!(txt.contains("Efficiency by job name:"));
    assert!(txt.contains("OOM x1: raise mem"));
    assert!(txt.contains("TIMEOUT x1: raise runtime"));
}

// ---------------------------------------------------------------- screens

#[test]
fn ui_queue_history_cursor_and_sort() {
    let mon = make_monitor();
    let mut app = App::new(mon, Hooks::default(), true);
    let term = render(&mut app, 160, 44);
    let lines = screen_lines(&term);
    eprintln!("{}", lines.join("\n"));
    assert!(lines[0].starts_with("  queuetop   mike@discovery   3 running   2 pending"));
    assert!(lines[1].contains("squeue") && lines[1].contains("sstat") && lines[1].contains("sacct"));
    let (qy, _) = find_line(&lines, "QUEUE   5").unwrap();
    let (ry, row) = lines.iter().enumerate().find(|(_, l)| regex::Regex::new(r"^  \*\s9849151\s+trimming\.2").unwrap().is_match(l)).unwrap();
    assert!(ry > qy && row.contains("RUNNING") && row.contains("60%"));
    let buf = term.backend().buffer();
    // queue rows: a coloured glyph in front, blinking for a running job; the text itself plain
    assert!(buf[(2, ry as u16)].modifier.contains(Modifier::REVERSED)); // cursor on the first row
    assert_eq!(buf[(2, ry as u16)].symbol(), "*");
    assert_eq!(buf[(2, ry as u16)].fg, Color::Green);
    assert!(buf[(2, ry as u16)].modifier.contains(Modifier::SLOW_BLINK));
    assert_eq!(buf[(4, ry as u16)].fg, Color::Reset);
    let (py, _) = lines.iter().enumerate().find(|(_, l)| regex::Regex::new(r"^  \^\s9849170\s+alignment\.8").unwrap().is_match(l)).unwrap();
    assert_eq!(buf[(2, py as u16)].fg, Color::Yellow);
    assert!(!buf[(2, py as u16)].modifier.contains(Modifier::SLOW_BLINK));
    assert_eq!(buf[(4, py as u16)].fg, Color::Reset);
    assert!(find_line(&lines, "FINISHED   5").is_some());
    // finished rows: the whole line coloured, red for ended badly, green for completed
    let (oy, _) = lines.iter().enumerate().find(|(_, l)| regex::Regex::new(r"^  !\s9849101\s+alignment\.7\s+OOM kill").unwrap().is_match(l)).unwrap();
    assert_eq!(buf[(2, oy as u16)].fg, Color::Red);
    assert_eq!(buf[(20, oy as u16)].fg, Color::Red);
    let (cy, _) = lines.iter().enumerate().find(|(_, l)| regex::Regex::new(r"^  \+\s9849100\s+trimming\.1\s+COMPLETED").unwrap().is_match(l)).unwrap();
    assert_eq!(buf[(20, cy as u16)].fg, Color::Green);
    assert!(lines[43].contains("x scancel") && lines[43].contains("c collapse") && lines[43].contains("C clear"));
    // sort by cpu% puts the busiest first
    app.handle_key(Key::Char('s'));
    app.handle_key(Key::Char('s'));
    assert_eq!(app.ui.sort, Sort::Cpu);
    let vis = app.ui.visible_jobs(&app.mon.snapshot());
    assert_eq!(vis[0].jobid, "9849151");
    // f moves the cursor to the finished list
    app.handle_key(Key::Char('f'));
    assert!(app.ui.focus_hist);
    let lines = screen_lines(&render(&mut app, 160, 44));
    assert!(find_line(&lines, "FINISHED   5   f back to queue").is_some());
    // filter
    app.handle_key(Key::Char('f'));
    app.handle_key(Key::Char('/'));
    for c in "ribo".chars() {
        app.handle_key(Key::Char(c));
    }
    app.handle_key(Key::Enter);
    assert_eq!(app.ui.visible_jobs(&app.mon.snapshot()).len(), 1);
    let lines = screen_lines(&render(&mut app, 160, 44));
    assert!(find_line(&lines, "QUEUE   1 of 5   filter 'ribo'").is_some());
    app.handle_key(Key::Escape);
    assert!(app.ui.filter.is_empty());
}

#[test]
fn ui_popup_with_scontrol_details_and_output_tail() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("log_trimming_9849151.out");
    fs::write(&out, "cutadapt 4.4\nProcessing reads on 4 cores ...\nERROR: adapter file not found\n").unwrap();
    std::env::set_var("QUEUETOP_FAKE_STDOUT", &out);
    let mon = make_monitor();
    let mut app = App::new(mon, Hooks::default(), true);
    render(&mut app, 160, 44);
    app.handle_key(Key::Enter);
    assert_eq!(app.ui.detail.as_ref().unwrap().jobid, "9849151");
    app.mon.wait_all(Some(10.0)); // scontrol
    let term = render(&mut app, 160, 44);
    let lines = screen_lines(&term);
    eprintln!("{}", lines.join("\n"));
    assert!(find_line(&lines, "job 9849151 (trimming.2)").is_some());
    assert!(find_line(&lines, "state  RUNNING").is_some());
    assert!(find_line(&lines, "request  4 cpus | 1 nodes | 58.0G memory").is_some());
    let (_, usage) = find_line(&lines, "usage  cpu 60% busy").unwrap();
    assert!(usage.contains("MaxRSS 3.0G (5% of request)"));
    assert!(find_line(&lines, "time  51:00 of 2:00:00 (42%)").is_some());
    assert!(find_line(&lines, "command  /dartfs/rc/lab/rnaseq/.snakemake/tmp.abc/snakejob.trimming.2.sh --cores 4").is_some());
    let (oy, orow) = find_line(&lines, "OUTPUT  ").unwrap();
    assert!(orow.contains("log_trimming_9849151.out"));
    let (ey, _) = find_line(&lines, "ERROR: adapter file not found").unwrap();
    assert!(ey > oy);
    assert_eq!(term.backend().buffer()[(lines[ey].find("ERROR").unwrap() as u16, ey as u16)].fg, Color::Red);
    // the screen behind the popup is dimmed to dark grey, highlights and all
    let behind = &term.backend().buffer()[(2, 0)]; // the header's first cell, normally cyan bold
    assert_eq!(behind.fg, Color::DarkGray);
    assert!(behind.modifier.contains(Modifier::DIM) && !behind.modifier.contains(Modifier::BOLD));
    // Esc closes
    app.handle_key(Key::Escape);
    assert!(app.ui.detail.is_none());
    std::env::remove_var("QUEUETOP_FAKE_STDOUT");
}

#[test]
fn ui_efficiency_screen_and_help() {
    let mon = make_monitor();
    let mut app = App::new(mon, Hooks::default(), true);
    app.handle_key(Key::Char('e'));
    let term = render(&mut app, 160, 44);
    let lines = screen_lines(&term);
    assert!(find_line(&lines, "EFFICIENCY   finished jobs grouped by name, last 24hours").is_some());
    let (ay, arow) = find_line(&lines, "alignment.7").unwrap();
    assert!(arow.contains("OOM x1: raise mem"));
    assert!(arow.contains("--mem=40G --time=1:00:00"), "{}", arow); // peak 32G + 25%, 40 min + 25% rounded to the quarter hour
    assert_eq!(term.backend().buffer()[(2, ay as u16)].fg, Color::Red);
    assert!(find_line(&lines, "qc_report").unwrap().1.contains("TIMEOUT x1"));
    app.handle_key(Key::Char('e'));
    app.handle_key(Key::Char('?'));
    let lines = screen_lines(&render(&mut app, 160, 44));
    assert!(find_line(&lines, &format!("queuetop {}", queuetop::VERSION)).is_some());
}

#[test]
fn scancel_confirmation_and_gate() {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("scancel.log");
    std::env::set_var("QUEUETOP_FAKE_SCANCEL_LOG", &log);
    let mon = make_monitor();
    let mut app = App::new(mon, Hooks::default(), true);
    render(&mut app, 160, 44);
    app.handle_key(Key::Char('j'));
    app.handle_key(Key::Char('x'));
    assert_eq!(app.ui.confirm.as_ref().unwrap().0, "9849161");
    let lines = screen_lines(&render(&mut app, 160, 44));
    assert!(lines[43].contains("scancel 9849161 (ribodetector.6)?"));
    app.handle_key(Key::Char('n'));
    app.process_requests();
    assert!(!log.exists());
    app.handle_key(Key::Char('x'));
    app.handle_key(Key::Char('y'));
    app.process_requests();
    assert_eq!(fs::read_to_string(&log).unwrap().trim(), "9849161");
    assert!(app.ui.current_notice().unwrap().text.contains("scancel 9849161: sent"));
    std::env::remove_var("QUEUETOP_FAKE_SCANCEL_LOG");
    // gated off
    let mut off = App::new(make_monitor(), Hooks::default(), false);
    render(&mut off, 160, 44);
    off.handle_key(Key::Char('x'));
    assert!(off.ui.confirm.is_none());
    assert!(off.ui.current_notice().unwrap().text.contains("--no-cancel"));
}

#[test]
fn app_notices_for_newly_finished_jobs() {
    let mut app = App::new(make_monitor(), Hooks::default(), true);
    app.refresh(); // primes with the five finished jobs
    assert!(app.ui.current_notice().is_none());
    app.refresh();
    assert!(app.ui.current_notice().is_none()); // nothing new
}

#[test]
fn finished_list_collapses_and_clears() {
    let mut app = App::new(make_monitor(), Hooks::default(), true);
    let lines = screen_lines(&render(&mut app, 160, 44));
    assert!(find_line(&lines, "FINISHED   5").is_some());
    // c folds the section to its title line and the queue takes the room
    app.handle_key(Key::Char('c'));
    let lines = screen_lines(&render(&mut app, 160, 44));
    assert!(find_line(&lines, "FINISHED   5 collapsed   c to show").is_some());
    assert!(find_line(&lines, "9849101").is_none());
    app.handle_key(Key::Char('c'));
    assert!(find_line(&screen_lines(&render(&mut app, 160, 44)), "9849101").is_some());
    // C clears what is listed; the history itself (and the efficiency view) keep it
    app.handle_key(Key::Char('f'));
    app.handle_key(Key::Char('C'));
    assert!(!app.ui.focus_hist);
    assert_eq!(app.ui.visible_hist(&app.mon.snapshot()).len(), 0);
    assert_eq!(app.mon.history().len(), 5);
    let lines = screen_lines(&render(&mut app, 160, 44));
    assert!(find_line(&lines, "FINISHED   0 of 5").is_some());
    assert!(find_line(&lines, "all cleared").is_some());
    assert!(app.ui.current_notice().unwrap().text.contains("cleared 5 finished jobs"));
}

#[test]
fn finished_jobs_age_out_of_the_window() {
    assert_eq!(since_seconds("now-24hours"), Some(86400.0));
    assert_eq!(since_seconds("now-7days"), Some(7.0 * 86400.0));
    assert_eq!(since_seconds("now-30minutes"), Some(1800.0));
    assert_eq!(since_seconds("2026-10-01"), None);
    let mon = make_monitor(); // --since now-24hours
    let newest = parse_slurm_time("2026-10-20T16:45:00").unwrap();
    assert_eq!(mon.history_at(newest + 3600.0).len(), 5);
    // 24 h after the OOM job ended only it is left; a day later nothing is
    assert_eq!(mon.history_at(newest + 86400.0).len(), 1);
    assert_eq!(mon.history_at(newest + 86400.0)[0].jobid, "9849101");
    assert_eq!(mon.history_at(newest + 2.0 * 86400.0).len(), 0);
}

#[test]
fn next_request_suggestions() {
    const M: u64 = 1024 * 1024;
    const G: u64 = 1024 * M;
    assert_eq!(suggest_mem(32 * G), 40 * G);
    assert_eq!(suggest_mem(500 * M), 640 * M); // 625M rounded up to 64M steps
    assert_eq!(suggest_mem(6 * G), 7 * G + 512 * M); // 7.5G in 256M steps
    assert_eq!(suggest_mem(1), 128 * M);
    assert_eq!(suggest_time(2400), 3600); // 40 min + 25% = 50 min -> next quarter hour
    assert_eq!(suggest_time(60), 900);
    assert_eq!(suggest_time(7200), 9000);
}

#[test]
fn arrays_fold_into_one_row() {
    assert_eq!(array_task_count("[1-3]"), 3);
    assert_eq!(array_task_count("[1-100%5]"), 100);
    assert_eq!(array_task_count("[2,5,7-9]"), 5);
    let task = |id: &str, state: &str, task: &str, cpus: i64, used: Option<i64>| QueueJob {
        jobid: id.to_string(),
        name: "arr".to_string(),
        state: state.to_string(),
        array_job: "500".to_string(),
        array_task: task.to_string(),
        cpus,
        mem_req: Some(1 << 30),
        time_used: used,
        ..Default::default()
    };
    let plain = QueueJob { jobid: "1".to_string(), name: "solo".to_string(), state: "RUNNING".to_string(), array_job: "1".to_string(), array_task: "N/A".to_string(), ..Default::default() };
    let jobs = vec![task("500_4", "RUNNING", "4", 2, Some(100)), plain.clone(), task("500_5", "RUNNING", "5", 2, Some(300)), task("500_[6-9]", "PENDING", "[6-9]", 2, Some(0))];
    let g = group_arrays(jobs);
    assert_eq!(g.len(), 2);
    assert_eq!(g[0].jobid, "500_*");
    assert!(g[0].is_group() && g[0].tasks.len() == 3);
    assert_eq!(g[0].state, "RUNNING");
    assert_eq!(g[0].reason, "2 running, 4 pending");
    assert_eq!((g[0].cpus, g[0].mem_req, g[0].time_used), (6, Some(3 << 30), Some(300)));
    assert_eq!(g[1].jobid, "1");
    // a lone task is not a group
    assert_eq!(group_arrays(vec![task("500_4", "RUNNING", "4", 2, None), plain]).len(), 2);
    // on screen: a folded row shows the counts, Enter expands it, a folds again
    let mut app = App::new(make_monitor(), Hooks::default(), true);
    assert_eq!(app.ui.visible_jobs(&app.mon.snapshot()).len(), 5); // the fakes hold one array row, so nothing folds
    app.handle_key(Key::Char('a'));
    assert!(!app.ui.collapse_arrays);
}

#[test]
fn finished_list_sorts() {
    let mut app = App::new(make_monitor(), Hooks::default(), true);
    render(&mut app, 160, 44);
    app.handle_key(Key::Char('f'));
    app.handle_key(Key::Char('s'));
    assert_eq!(app.ui.hist_sort, HistSort::Name);
    let names: Vec<String> = app.ui.visible_hist(&app.mon.snapshot()).iter().map(|h| h.name.clone()).collect();
    assert_eq!(names, ["alignment.7", "notebook", "qc_report", "trimming.1", "trimming.3"]);
    app.handle_key(Key::Char('s'));
    let v = app.ui.visible_hist(&app.mon.snapshot());
    assert!(v[0].is_bad() && v[1].is_bad() && v[2].is_bad() && !v[3].is_bad());
    let lines = screen_lines(&render(&mut app, 160, 44));
    assert!(find_line(&lines, "FINISHED   5   by state   f back to queue").is_some());
    // the queue sort is untouched
    assert_eq!(app.ui.sort, Sort::State);
}

#[test]
fn popup_shows_the_batch_script() {
    let mut app = App::new(make_monitor(), Hooks::default(), true);
    render(&mut app, 160, 44);
    app.handle_key(Key::Enter);
    app.handle_key(Key::Char('b'));
    assert!(app.ui.detail.as_ref().unwrap().show_script);
    app.mon.wait_all(Some(10.0));
    let lines = screen_lines(&render(&mut app, 160, 44));
    assert!(find_line(&lines, "SCRIPT  scontrol write batch_script 9849151").is_some());
    let (sy, _) = find_line(&lines, "#SBATCH --mem=58G").unwrap();
    let (cy, _) = find_line(&lines, "cutadapt -j 4 -o trimmed.fq reads.fq").unwrap();
    assert!(cy > sy);
    app.handle_key(Key::Char('b'));
    assert!(!app.ui.detail.as_ref().unwrap().show_script);
}

#[test]
fn cleared_jobs_are_remembered() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("cache").join("cleared");
    let mut app = App::new(make_monitor(), Hooks::default(), true);
    app.ui.cleared_file = Some(file.clone());
    render(&mut app, 160, 44);
    app.handle_key(Key::Char('C'));
    let text = fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().count(), 5);
    assert!(text.lines().any(|l| l.starts_with("9849101\t")));
    // a fresh ui reads them back
    let mut again = App::new(make_monitor(), Hooks::default(), true);
    again.ui.cleared_file = Some(file.clone());
    again.ui.load_cleared();
    assert_eq!(again.ui.visible_hist(&again.mon.snapshot()).len(), 0);
    // entries older than a week are dropped on the next save
    again.ui.dismissed.insert("old".to_string(), parse_slurm_time(NOW).unwrap() - 8.0 * 86400.0);
    again.ui.save_cleared();
    assert!(!fs::read_to_string(&file).unwrap().contains("old"));
}

#[test]
fn mouse_selects_and_opens() {
    let mut app = App::new(make_monitor(), Hooks::default(), true);
    let lines = screen_lines(&render(&mut app, 160, 44));
    let (ry, _) = lines.iter().enumerate().find(|(_, l)| l.contains("ribodetector.6")).unwrap();
    app.ui.handle_mouse(Mouse::Click(10, ry as i32), &mut app.mon);
    assert_eq!(app.ui.cursor, 1);
    assert!(app.ui.detail.is_none());
    app.ui.handle_mouse(Mouse::Click(10, ry as i32), &mut app.mon); // the second click opens
    assert_eq!(app.ui.detail.as_ref().unwrap().jobid, "9849161");
    app.handle_key(Key::Escape);
    // a click in the finished list moves the cursor there
    let (oy, _) = lines.iter().enumerate().find(|(_, l)| l.contains("alignment.7")).unwrap();
    app.ui.handle_mouse(Mouse::Click(10, oy as i32), &mut app.mon);
    assert!(app.ui.focus_hist && app.ui.cursor == 0);
    // the wheel moves the cursor
    app.handle_key(Key::Char('f'));
    render(&mut app, 160, 44);
    app.ui.handle_mouse(Mouse::Scroll(3), &mut app.mon);
    assert_eq!(app.ui.cursor, 3);
    app.ui.handle_mouse(Mouse::Scroll(-3), &mut app.mon);
    assert_eq!(app.ui.cursor, 0);
}

#[test]
fn f_needs_finished_jobs() {
    let mut app = App::new(make_monitor(), Hooks::default(), true);
    render(&mut app, 160, 44);
    app.handle_key(Key::Char('C'));
    app.handle_key(Key::Char('f'));
    assert!(!app.ui.focus_hist);
    assert!(app.ui.current_notice().unwrap().text.contains("no finished jobs"));
}

#[test]
fn config_file_becomes_arguments() {
    let text = "# defaults\nsince = \"now-7days\"\nbell = true\nno_sacct = false\nsstat-interval = 30\nuser = 'alice'\n\nbad line\n";
    assert_eq!(config_args(text), ["--since=now-7days", "--bell", "--sstat-interval=30", "--user=alice"]);
}

#[test]
fn hooks_fire_once_per_poll() {
    let evs = vec![
        Event { jobid: "1".into(), name: "a".into(), state: "OOM kill".into(), bad: true },
        Event { jobid: "2".into(), name: "b".into(), state: "COMPLETED".into(), bad: false },
    ];
    assert_eq!(describe_all(&evs[..1]), "job 1 (a) ended: OOM kill");
    assert_eq!(describe_all(&evs), "2 jobs ended, 1 badly: 1 (a) OOM kill, 2 (b) completed");
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("hook.txt");
    let hooks = Hooks { bell: false, command: Some(format!("echo \"$QUEUETOP_EVENT $QUEUETOP_COUNT $QUEUETOP_JOBIDS\" > '{}'", out.display())) };
    hooks.fire(&evs).unwrap();
    let mut text = String::new();
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(40));
        text = fs::read_to_string(&out).unwrap_or_default();
        if !text.is_empty() {
            break;
        }
    }
    assert_eq!(text.trim(), "job_failed 2 1 2");
}

#[test]
fn once_and_json_binaries() {
    let fake = fakes();
    let path = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap_or_default());
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_queuetop"))
        .args(["--once", "--user", "mike"])
        .env("PATH", &path)
        .env("QUEUETOP_CONFIG", "")
        .env("QUEUETOP_NOW", format!("{}", parse_slurm_time(NOW).unwrap()))
        .env("SLURM_CLUSTER_NAME", "discovery")
        .output()
        .unwrap();
    assert!(out.status.success());
    let txt = String::from_utf8_lossy(&out.stdout);
    assert!(txt.starts_with("queuetop   mike@discovery"), "{}", txt);
    assert!(txt.contains("Queue (5):") && txt.contains("Finished (5):"));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_queuetop"))
        .args(["--json", "--user", "mike", "--since", "now-7days"])
        .env("PATH", &path)
        .env("QUEUETOP_CONFIG", "")
        .env("QUEUETOP_NOW", format!("{}", parse_slurm_time(NOW).unwrap()))
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["user"], "mike");
    assert_eq!(v["summary"]["running"], 3);
    assert_eq!(v["queue"].as_array().unwrap().len(), 5);
    assert_eq!(v["queue"][0]["cpu_pct"].as_f64().unwrap().round(), 1.0);
    assert_eq!(v["finished"].as_array().unwrap().len(), 5);
    assert_eq!(v["finished"][0]["short"], "OOM kill");
    assert_eq!(v["finished"][0]["oom"], true);
    assert_eq!(v["sources"]["sacct"]["since"], "now-7days");
    assert!(v["efficiency"].as_array().unwrap().iter().any(|e| e["note"] == "OOM x1: raise mem" && e["next_request"] == "--mem=40G --time=1:00:00"));
}
