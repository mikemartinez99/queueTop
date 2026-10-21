//! queuetop tests: parsers, the monitor against the fake Slurm tools, the
//! screens, and the --once / --json binaries.

use std::fs;
use std::path::{Path, PathBuf};

use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use hisstop::events::Hooks;
use queuetop::app::App;
use queuetop::model::{cpu_pct, efficiency_by_name, mem_pct, summary};
use queuetop::monitor::{Monitor, Options};
use queuetop::slurm::{parse_sacct_hist, parse_scontrol, parse_slurm_time, parse_squeue, parse_sstat, Tools};
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

/// A monitor that has polled every fake once.
fn make_monitor() -> Monitor {
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
    assert!((t.cpu_secs - (2.0 * 3600.0 + 2.0 * 60.0 + 0.001)).abs() < 1e-6);
    assert_eq!(t.max_rss, Some(3145728 * 1024));
    assert_eq!(t.steps, 2);
    let r = &st["9849161"];
    assert!((r.cpu_secs - 38.401).abs() < 1e-6); // the unsampled step's NO_VAL sentinel is ignored
    assert_eq!(r.max_rss, Some(52428800 * 1024));
    assert_eq!(r.steps, 3);
    assert!(queuetop::slurm::sane_cpu_time("213503982334-14:25:51").is_none());
    assert_eq!(queuetop::slurm::sane_cpu_time("1-02:03:04.5"), Some(86400.0 + 7384.5));
}

#[test]
fn absurd_cpu_readings_are_not_shown() {
    use queuetop::slurm::{HistJob, QueueJob, Stat};
    let job = QueueJob { jobid: "1".into(), time_used: Some(600), cpus: 4, ..Default::default() };
    let stat = Stat { jobid: "1".into(), cpu_secs: 1.8e16, max_rss: None, steps: 1 };
    assert!(cpu_pct(&job, Some(&stat), None).is_none());
    let ok = Stat { jobid: "1".into(), cpu_secs: 1200.0, max_rss: None, steps: 1 };
    assert!((cpu_pct(&job, Some(&ok), None).unwrap() - 0.5).abs() < 1e-9);
    // an empty sstat falls through to sacct's TotalCPU
    let mut h = HistJob::default();
    h.row.total_cpu = Some(2400.0);
    let zero = Stat { jobid: "1".into(), cpu_secs: 0.0, max_rss: None, steps: 1 };
    assert!((cpu_pct(&job, Some(&zero), Some(&h)).unwrap() - 1.0).abs() < 1e-9);
}

#[test]
fn sacct_history_parses_and_groups() {
    let h = parse_sacct_hist(&run_fake("sacct"));
    assert_eq!(h.len(), 6);
    let oom = &h["9849101"];
    assert_eq!(oom.name, "alignment.7");
    assert_eq!(oom.row.state, "OUT_OF_MEMORY");
    assert_eq!(oom.row.max_rss, Some(33554432 * 1024));
    assert_eq!(oom.row.req_mem, Some(32 << 30));
    assert!(oom.row.mem_use().unwrap() > 0.99);
    assert_eq!(oom.wait(), Some(300.0));
    assert_eq!(oom.partition, "standard");
    let running = &h["9849151"];
    assert!(!running.row.is_final());
    assert!(running.end.is_none());
    let effs = efficiency_by_name(&h.values().cloned().collect::<Vec<_>>());
    let names: Vec<&str> = effs.iter().map(|e| e.rule.as_str()).collect();
    assert!(names.contains(&"trimming.1") && names.contains(&"alignment.7"));
    let al = effs.iter().find(|e| e.rule == "alignment.7").unwrap();
    assert_eq!(al.oom, 1);
    assert_eq!(al.note().1, "failed");
    let qc = effs.iter().find(|e| e.rule == "qc_report").unwrap();
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
    let s = mon.stat_for("9849151").unwrap();
    assert_eq!(s.max_rss, Some(3145728 * 1024));
    let hist = mon.history();
    assert_eq!(hist.len(), 5); // the running one is not history
    assert_eq!(hist[0].row.jobid, "9849101"); // most recently ended first
    let t = &jobs[0];
    let cpu = cpu_pct(t, mon.stat_for(&t.jobid).as_ref(), None).unwrap();
    assert!((cpu - (7320.001 / (3060.0 * 4.0))).abs() < 1e-6); // ~60%
    let mem = mem_pct(t, mon.stat_for(&t.jobid).as_ref(), None).unwrap();
    assert!((mem - 3.0 / 58.0).abs() < 1e-6);
    let sum = summary(&jobs, &hist);
    assert_eq!((sum.running, sum.pending, sum.other), (3, 2, 0));
    assert_eq!(sum.cpus_running, 16);
    assert_eq!((sum.finished, sum.finished_bad), (5, 3));
    assert_eq!(sum.next_start, parse_slurm_time("2026-10-20T18:30:00"));
    // sacct TotalCPU stands in when sstat has nothing
    let h = mon.hist_for("9849151").unwrap();
    assert!(cpu_pct(t, None, Some(&h)).is_some());
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
    assert!(regex::Regex::new(r"9849151\s+trimming\.2\s+standard\s+RUNNING\s+51:00\s+2:00:00\s+4\s+60%\s+58\.0G\s+3\.0G\s+5%\s+t03").unwrap().is_match(&txt));
    assert!(regex::Regex::new(r"9849170\s+alignment\.8\s+standard\s+PENDING\s+0:00\s+2:00:00\s+8\s+-\s+32\.0G\s+-\s+-\s+\(Priority\)").unwrap().is_match(&txt));
    assert!(txt.contains("Finished (5):"));
    assert!(regex::Regex::new(r"9849101\s+alignment\.7\s+OUT_OF_MEMORY\s+40:00\s+2:00:00\s+32\.0G\s+32\.0G\s+100%\s+47%").unwrap().is_match(&txt));
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
    let (ry, row) = lines.iter().enumerate().find(|(_, l)| regex::Regex::new(r"^  9849151\s+trimming\.2").unwrap().is_match(l)).unwrap();
    assert!(ry > qy && row.contains("RUNNING") && row.contains("60%"));
    let buf = term.backend().buffer();
    assert!(buf[(2, ry as u16)].modifier.contains(Modifier::REVERSED)); // cursor on the first row
    assert_eq!(buf[(2, ry as u16)].fg, Color::Green);
    let (py, _) = lines.iter().enumerate().find(|(_, l)| regex::Regex::new(r"^  9849170\s+alignment\.8").unwrap().is_match(l)).unwrap();
    assert_eq!(buf[(2, py as u16)].fg, Color::Yellow);
    assert!(find_line(&lines, "FINISHED   5").is_some());
    let (oy, _) = lines.iter().enumerate().find(|(_, l)| regex::Regex::new(r"^  9849101\s+alignment\.7\s+OUT_OF_MEMORY").unwrap().is_match(l)).unwrap();
    assert_eq!(buf[(2, oy as u16)].fg, Color::Red);
    assert!(lines[43].contains("x scancel"));
    // sort by cpu% puts the busiest first
    app.handle_key(Key::Char('s'));
    app.handle_key(Key::Char('s'));
    assert_eq!(app.ui.sort, Sort::Cpu);
    let vis = app.ui.visible_jobs(&app.mon);
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
    assert_eq!(app.ui.visible_jobs(&app.mon).len(), 1);
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
    assert!(find_line(&lines, "job 9849151 \u{b7} trimming.2").is_some());
    assert!(find_line(&lines, "state  RUNNING").is_some());
    assert!(find_line(&lines, "request  4 cpus \u{b7} 1 nodes \u{b7} 58.0G memory").is_some());
    let (_, usage) = find_line(&lines, "usage  cpu 60% busy").unwrap();
    assert!(usage.contains("MaxRSS 3.0G (5% of request)"));
    assert!(find_line(&lines, "time  51:00 of 2:00:00 (42%)").is_some());
    assert!(find_line(&lines, "command  /dartfs/rc/lab/rnaseq/.snakemake/tmp.abc/snakejob.trimming.2.sh --cores 4").is_some());
    let (oy, orow) = find_line(&lines, "OUTPUT  ").unwrap();
    assert!(orow.contains("log_trimming_9849151.out"));
    let (ey, _) = find_line(&lines, "ERROR: adapter file not found").unwrap();
    assert!(ey > oy);
    assert_eq!(term.backend().buffer()[(lines[ey].find("ERROR").unwrap() as u16, ey as u16)].fg, Color::Red);
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
fn once_and_json_binaries() {
    let fake = fakes();
    let path = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap_or_default());
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_queuetop"))
        .args(["--once", "--user", "mike"])
        .env("PATH", &path)
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
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["user"], "mike");
    assert_eq!(v["summary"]["running"], 3);
    assert_eq!(v["queue"].as_array().unwrap().len(), 5);
    assert_eq!(v["queue"][0]["cpu_pct"].as_f64().unwrap().round(), 1.0);
    assert_eq!(v["finished"].as_array().unwrap().len(), 5);
    assert_eq!(v["finished"][0]["short"], "OUT_OF_MEMORY");
    assert_eq!(v["sources"]["sacct"]["since"], "now-7days");
    assert!(v["efficiency"].as_array().unwrap().iter().any(|e| e["note"] == "OOM x1: raise mem"));
}
