//! The Slurm commands queuetop runs, the parsers for their output, and the
//! unit helpers (elapsed times, memory strings, CPU seconds) they share.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDateTime, TimeZone};
use indexmap::IndexMap;
use regex::Regex;

use crate::text::{fmt_dur, fmt_mem};

const TIMEOUT: Duration = Duration::from_secs(45);

// ------------------------------------------------------------------ running commands

/// Which executables to run; None = look on PATH. Tests point these at fakes.
#[derive(Clone, Debug, Default)]
pub struct Tools {
    pub squeue: Option<PathBuf>,
    pub sstat: Option<PathBuf>,
    pub sacct: Option<PathBuf>,
    pub scontrol: Option<PathBuf>,
    pub scancel: Option<PathBuf>,
}

impl Tools {
    /// Every tool from one directory (the test fakes).
    pub fn from_dir(dir: &Path) -> Tools {
        Tools {
            squeue: Some(dir.join("squeue")),
            sstat: Some(dir.join("sstat")),
            sacct: Some(dir.join("sacct")),
            scontrol: Some(dir.join("scontrol")),
            scancel: Some(dir.join("scancel")),
        }
    }
}

/// `shutil.which(name)`.
pub fn find_exe(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let p = dir.join(name);
        if is_executable(&p) {
            return Some(p);
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(md) => md.is_file() && md.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Run a command, capturing stdout and stderr, killing it after `timeout`.
/// Returns (stdout, stderr, exited zero).
pub fn run_with_timeout(exe: &Path, args: &[&str], timeout: Duration) -> Result<(String, String, bool), String> {
    let mut child = Command::new(exe).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_t = thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out_pipe.read_to_end(&mut s);
        String::from_utf8_lossy(&s).into_owned()
    });
    let err_t = thread::spawn(move || {
        let mut s = Vec::new();
        let _ = err_pipe.read_to_end(&mut s);
        String::from_utf8_lossy(&s).into_owned()
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {} seconds", timeout.as_secs()));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok((out_t.join().unwrap_or_default(), err_t.join().unwrap_or_default(), status.success()))
}

fn run(name: &str, over: &Option<PathBuf>, args: &[&str]) -> Result<String, String> {
    let exe = over.clone().or_else(|| find_exe(name)).ok_or_else(|| format!("{} not found", name))?;
    let (out, err, ok) = run_with_timeout(&exe, args, TIMEOUT).map_err(|e| format!("{}: {}", name, e))?;
    if !ok {
        let last = err.trim().lines().last().unwrap_or("failed");
        return Err(format!("{}: {}", name, last.chars().take(80).collect::<String>()));
    }
    Ok(out)
}

// ------------------------------------------------------------------ units

static RE_ELAPSED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:(\d+)-)?(?:(\d+):)?(\d+):(\d+)$").unwrap());
static RE_MEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([\d.]+)\s*([KMGTP]?)([nc]?)$").unwrap());

/// `12:34`, `1:02:03`, `2-03:04:05` -> seconds; `UNLIMITED`, `Partition_Limit`, `` -> None.
pub fn parse_slurm_elapsed(text: &str) -> Option<i64> {
    let c = RE_ELAPSED.captures(text.trim())?;
    let num = |i: usize| c.get(i).map(|m| m.as_str().parse::<i64>().unwrap_or(0)).unwrap_or(0);
    Some(num(1) * 86400 + num(2) * 3600 + num(3) * 60 + num(4))
}

/// CPU time `12:00.000`, `1:02:03`, `1-02:03:04.5` -> seconds.
pub fn parse_cpu_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let secs = parse_slurm_elapsed(whole)? as f64;
    let frac: f64 = if frac.is_empty() { 0.0 } else { format!("0.{}", frac).parse().unwrap_or(0.0) };
    Some(secs + frac)
}

/// `394172K`, `1.5G`, `8000M`, `0`, `` -> bytes. A trailing n/c (per node /
/// per CPU, old Slurm ReqMem) is accepted and ignored here; see parse_req_mem.
pub fn parse_mem(s: &str) -> Option<u64> {
    let c = RE_MEM.captures(s.trim())?;
    let n: f64 = c[1].parse().ok()?;
    let mult: f64 = match &c[2] {
        "" | "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        "T" => 1024.0_f64.powi(4),
        "P" => 1024.0_f64.powi(5),
        _ => return None,
    };
    Some((n * mult).round() as u64)
}

/// ReqMem for the whole job: `2000Mc` is per allocated CPU, `8000Mn` per node
/// (one node assumed), plain `8000M` is the job total (Slurm >= 21.08).
pub fn parse_req_mem(s: &str, alloc_cpus: Option<i64>) -> Option<u64> {
    let c = RE_MEM.captures(s.trim())?;
    let base = parse_mem(&format!("{}{}", &c[1], &c[2]))?;
    if base == 0 {
        return None;
    }
    Some(match &c[3] {
        "c" => base * alloc_cpus.unwrap_or(1).max(1) as u64,
        _ => base,
    })
}

/// `2026-10-20T17:05:49` -> local epoch; `N/A`, `Unknown`, `` -> None.
pub fn parse_slurm_time(s: &str) -> Option<f64> {
    let naive = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%dT%H:%M:%S").ok()?;
    Local.from_local_datetime(&naive).earliest().map(|t| t.timestamp() as f64)
}

/// Sort key that puts numeric job ids in numeric order and the rest after.
pub fn jobkey(jobid: &str) -> (u8, Option<i64>, String) {
    if !jobid.is_empty() && jobid.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = jobid.parse::<i64>() {
            return (0, Some(n), String::new());
        }
    }
    (1, None, jobid.to_string())
}

// ------------------------------------------------------------------ squeue

pub const SQUEUE_FORMAT: &str = "%i|%j|%T|%P|%M|%l|%C|%m|%D|%N|%r|%V|%S|%Q|%a|%q|%Z|%F|%K|%E";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueueJob {
    pub jobid: String,
    pub name: String,
    pub state: String,
    pub partition: String,
    pub time_used: Option<i64>,
    pub time_limit: Option<i64>,
    pub cpus: i64,
    /// bytes requested for the job
    pub mem_req: Option<u64>,
    pub nodes: i64,
    pub nodelist: String,
    pub reason: String,
    pub submit: Option<f64>,
    /// actual start when running, Slurm's estimate when pending
    pub start: Option<f64>,
    pub priority: i64,
    pub account: String,
    pub qos: String,
    pub workdir: String,
    pub array_job: String,
    pub array_task: String,
    pub dependency: String,
    /// the tasks behind a collapsed array row (empty for a plain job)
    pub tasks: Vec<QueueJob>,
}

impl QueueJob {
    pub fn is_group(&self) -> bool {
        !self.tasks.is_empty()
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state.as_str(), "RUNNING" | "COMPLETING" | "CONFIGURING")
    }

    pub fn is_pending(&self) -> bool {
        self.state == "PENDING"
    }

    pub fn time_use(&self) -> Option<f64> {
        match (self.time_used, self.time_limit) {
            (Some(u), Some(l)) if l > 0 => Some(u as f64 / l as f64),
            _ => None,
        }
    }

    /// Expected end: start + limit.
    pub fn deadline(&self) -> Option<f64> {
        Some(self.start? + self.time_limit? as f64)
    }
}

pub fn parse_squeue(text: &str) -> Vec<QueueJob> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.trim_end_matches('\r').split('|').collect();
        if f.len() < 11 || f[0].is_empty() {
            continue;
        }
        let g = |i: usize| f.get(i).map(|s| s.trim()).unwrap_or("");
        let cpus: i64 = g(6).parse().unwrap_or(0);
        out.push(QueueJob {
            jobid: g(0).to_string(),
            name: g(1).to_string(),
            state: g(2).to_uppercase(),
            partition: g(3).to_string(),
            time_used: parse_slurm_elapsed(g(4)),
            time_limit: parse_slurm_elapsed(g(5)),
            cpus,
            mem_req: parse_req_mem(g(7), Some(cpus)),
            nodes: g(8).parse().unwrap_or(0),
            nodelist: g(9).to_string(),
            reason: g(10).to_string(),
            submit: parse_slurm_time(g(11)),
            start: parse_slurm_time(g(12)),
            priority: g(13).parse().unwrap_or(0),
            account: g(14).to_string(),
            qos: g(15).to_string(),
            workdir: g(16).to_string(),
            array_job: g(17).to_string(),
            array_task: g(18).to_string(),
            dependency: g(19).to_string(),
            tasks: Vec::new(),
        });
    }
    out
}

pub fn squeue_user(tools: &Tools, user: &str) -> Result<Vec<QueueJob>, String> {
    let out = run("squeue", &tools.squeue, &["-h", "-u", user, "-o", SQUEUE_FORMAT])?;
    Ok(parse_squeue(&out))
}

// ------------------------------------------------------------------ sstat

pub const SSTAT_FORMAT: &str = "JobID,AveCPU,MaxRSS,AveRSS,NTasks,TRESUsageInTot";

/// Slurm prints its "no value" sentinel (2^64 - 2 milliseconds) for fields it has not
/// sampled yet, as a days-hours-minutes string in the 10^16 seconds range. Anything
/// above a million years is that sentinel, not a measurement.
const SANE_SECONDS: f64 = 3.2e13;

/// A CPU time that is a real measurement, not Slurm's NO_VAL sentinel.
pub fn sane_cpu_time(s: &str) -> Option<f64> {
    parse_cpu_time(s).filter(|c| *c >= 0.0 && *c < SANE_SECONDS)
}

/// Live usage of a running job, summed over its steps.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stat {
    pub jobid: String,
    /// CPU seconds consumed so far: TRESUsageInTot's cpu= when present, else
    /// AveCPU x NTasks, summed over steps. None when Slurm gave no usable number.
    pub cpu_secs: Option<f64>,
    pub max_rss: Option<u64>,
    pub steps: usize,
}

/// `cpu=01:02:03,energy=0,fs/disk=123` -> the cpu entry as seconds.
pub fn tres_cpu_seconds(tres: &str) -> Option<f64> {
    tres.split(',').find_map(|kv| kv.trim().strip_prefix("cpu=")).and_then(sane_cpu_time)
}

pub fn parse_sstat(text: &str) -> IndexMap<String, Stat> {
    let mut out: IndexMap<String, Stat> = IndexMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.trim_end_matches('\r').split('|').collect();
        if f.len() < 5 || f[0].is_empty() {
            continue;
        }
        let base = f[0].split('.').next().unwrap_or(f[0]).to_string();
        let st = out.entry(base.clone()).or_insert_with(|| Stat { jobid: base.clone(), ..Default::default() });
        let ntasks: f64 = f[4].trim().parse().unwrap_or(1.0);
        let step_cpu = f.get(5).and_then(|t| tres_cpu_seconds(t)).or_else(|| sane_cpu_time(f[1]).map(|c| c * ntasks.clamp(1.0, 100_000.0)));
        if let Some(c) = step_cpu {
            st.cpu_secs = Some(st.cpu_secs.unwrap_or(0.0) + c);
        }
        if let Some(r) = parse_mem(f[2]).filter(|v| *v > 0 && (*v as f64) < 1e18) {
            st.max_rss = Some(st.max_rss.map_or(r, |c| c.max(r)));
        }
        st.steps += 1;
    }
    out
}

pub fn sstat_jobs(tools: &Tools, ids: &[String]) -> Result<IndexMap<String, Stat>, String> {
    if ids.is_empty() {
        return Ok(IndexMap::new());
    }
    let joined = ids.join(",");
    let format = format!("--format={}", SSTAT_FORMAT);
    // sstat exits non-zero when any id is not running; its stdout is still useful
    let exe = tools.sstat.clone().or_else(|| find_exe("sstat")).ok_or_else(|| "sstat not found".to_string())?;
    let (out, err, ok) = run_with_timeout(&exe, &["-a", "-n", "-P", "-j", &joined, &format], TIMEOUT).map_err(|e| format!("sstat: {}", e))?;
    let parsed = parse_sstat(&out);
    if !ok && parsed.is_empty() {
        let last = err.trim().lines().last().unwrap_or("failed");
        return Err(format!("sstat: {}", last.chars().take(80).collect::<String>()));
    }
    Ok(parsed)
}

// ------------------------------------------------------------------ sacct

pub const SACCT_FORMAT: &str =
    "JobID,JobName,State,ExitCode,Elapsed,Timelimit,ReqMem,MaxRSS,TotalCPU,AllocCPUS,NodeList,Partition,Submit,Start,End";

const FINAL: &[&str] = &["COMPLETED", "FAILED", "TIMEOUT", "OUT_OF_MEMORY", "NODE_FAIL", "CANCELLED", "PREEMPTED", "DEADLINE", "BOOT_FAIL", "REVOKED"];

/// A job as the accounting database remembers it (steps folded in).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistJob {
    pub jobid: String,
    pub name: String,
    /// normalised: "CANCELLED by 1000" -> "CANCELLED"
    pub state: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub elapsed: Option<i64>,
    pub timelimit: Option<i64>,
    /// bytes requested for the whole job
    pub req_mem: Option<u64>,
    /// bytes, the largest MaxRSS over the job's steps
    pub max_rss: Option<u64>,
    /// CPU seconds consumed
    pub total_cpu: Option<f64>,
    pub alloc_cpus: Option<i64>,
    pub nodelist: String,
    pub partition: String,
    pub submit: Option<f64>,
    pub start: Option<f64>,
    pub end: Option<f64>,
}

impl HistJob {
    pub fn is_final(&self) -> bool {
        FINAL.contains(&self.state.as_str())
    }

    pub fn is_bad(&self) -> bool {
        self.is_final() && self.state != "COMPLETED"
    }

    pub fn is_oom(&self) -> bool {
        self.state == "OUT_OF_MEMORY"
    }

    /// Elapsed / Timelimit, when both are known.
    pub fn time_use(&self) -> Option<f64> {
        match (self.elapsed, self.timelimit) {
            (Some(e), Some(l)) if l > 0 => Some(e as f64 / l as f64),
            _ => None,
        }
    }

    /// MaxRSS / ReqMem, when both are known.
    pub fn mem_use(&self) -> Option<f64> {
        match (self.max_rss, self.req_mem) {
            (Some(r), Some(q)) if q > 0 => Some(r as f64 / q as f64),
            _ => None,
        }
    }

    /// TotalCPU / (Elapsed x AllocCPUS): how busy the allocated cores were.
    pub fn cpu_eff(&self) -> Option<f64> {
        match (self.total_cpu, self.elapsed, self.alloc_cpus) {
            (Some(c), Some(e), Some(n)) if e > 0 && n > 0 => Some(c / (e as f64 * n as f64)),
            _ => None,
        }
    }

    /// More than 90% of the time or memory request used.
    pub fn near_limit(&self) -> bool {
        self.time_use().is_some_and(|u| u > 0.9) || self.mem_use().is_some_and(|u| u > 0.9)
    }

    /// Queue wait in seconds.
    pub fn wait(&self) -> Option<f64> {
        Some(self.start? - self.submit?)
    }

    /// One token for a table cell: `COMPLETED`, `OOM kill`, `TIMEOUT`, `exit 1`, `signal 9` ...
    pub fn short(&self) -> String {
        match self.state.as_str() {
            "OUT_OF_MEMORY" => "OOM kill".to_string(),
            "FAILED" => match (self.exit_code, self.signal) {
                (Some(0), Some(s)) if s != 0 => format!("signal {}", s),
                (Some(c), _) => format!("exit {}", c),
                _ => "FAILED".to_string(),
            },
            s => s.to_string(),
        }
    }

    /// One line for the popup.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = vec![if self.is_oom() { "OUT_OF_MEMORY (OOM kill)".to_string() } else { self.state.clone() }];
        if let (Some(c), Some(s)) = (self.exit_code, self.signal) {
            if c != 0 || s != 0 {
                parts.push(if s != 0 { format!("exit {}:{} (signal {})", c, s, s) } else { format!("exit {}", c) });
            }
        }
        if let Some(e) = self.elapsed {
            let mut t = fmt_dur(Some(e as f64));
            if let Some(l) = self.timelimit {
                t.push_str(&format!(" of {}", fmt_dur(Some(l as f64))));
                if let Some(u) = self.time_use() {
                    t.push_str(&format!(" ({:.0}%)", u * 100.0));
                }
            }
            parts.push(t);
        }
        if let Some(r) = self.max_rss {
            let mut m = format!("MaxRSS {}", fmt_mem(r));
            if let Some(q) = self.req_mem {
                m.push_str(&format!(" of {}", fmt_mem(q)));
                if let Some(u) = self.mem_use() {
                    m.push_str(&format!(" ({:.0}%)", u * 100.0));
                }
            }
            parts.push(m);
        } else if let Some(q) = self.req_mem {
            parts.push(format!("ReqMem {}", fmt_mem(q)));
        }
        if let Some(n) = self.alloc_cpus.filter(|n| *n > 0) {
            let mut c = format!("{} CPU{}", n, if n == 1 { "" } else { "s" });
            if let Some(eff) = self.cpu_eff() {
                c.push_str(&format!(" at {:.0}%", eff * 100.0));
            }
            if !self.nodelist.is_empty() && self.nodelist != "None assigned" {
                c.push_str(&format!(" on {}", self.nodelist));
            }
            parts.push(c);
        }
        parts.join(" | ")
    }
}

fn normalise_state(s: &str) -> String {
    s.split_whitespace().next().unwrap_or("").trim_end_matches('+').to_uppercase()
}

/// `-n -P` sacct output -> {jobid: job}, steps folded into their job.
pub fn parse_sacct_hist(text: &str) -> IndexMap<String, HistJob> {
    let mut out: IndexMap<String, HistJob> = IndexMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.trim_end_matches('\r').split('|').collect();
        if f.len() < 15 || f[0].is_empty() {
            continue;
        }
        let (base, is_step) = match f[0].split_once('.') {
            Some((b, _)) => (b.to_string(), true),
            None => (f[0].to_string(), false),
        };
        let h = out.entry(base.clone()).or_insert_with(|| HistJob { jobid: base.clone(), ..Default::default() });
        if let Some(r) = parse_mem(f[7]).filter(|v| *v > 0) {
            h.max_rss = Some(h.max_rss.map_or(r, |c| c.max(r)));
        }
        if is_step {
            continue;
        }
        h.name = f[1].to_string();
        h.state = normalise_state(f[2]);
        if let Some((c, s)) = f[3].split_once(':') {
            h.exit_code = c.trim().parse().ok();
            h.signal = s.trim().parse().ok();
        }
        h.elapsed = parse_slurm_elapsed(f[4]);
        h.timelimit = parse_slurm_elapsed(f[5]);
        h.alloc_cpus = f[9].trim().parse().ok();
        h.req_mem = parse_req_mem(f[6], h.alloc_cpus);
        h.total_cpu = sane_cpu_time(f[8]);
        h.nodelist = f[10].trim().to_string();
        h.partition = f[11].trim().to_string();
        h.submit = parse_slurm_time(f[12]);
        h.start = parse_slurm_time(f[13]);
        h.end = parse_slurm_time(f[14]);
    }
    out
}

pub fn sacct_user(tools: &Tools, user: &str, since: &str) -> Result<IndexMap<String, HistJob>, String> {
    let format = format!("--format={}", SACCT_FORMAT);
    let out = run("sacct", &tools.sacct, &["-n", "-P", "-u", user, "-S", since, &format])?;
    Ok(parse_sacct_hist(&out))
}

/// `now-24hours`, `now-7days`, `now-30minutes` -> seconds; anything else -> None.
pub fn since_seconds(since: &str) -> Option<f64> {
    let rest = since.trim().strip_prefix("now-")?;
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    let n: f64 = rest[..digits].parse().ok()?;
    let unit = match rest[digits..].trim() {
        "minute" | "minutes" => 60.0,
        "hour" | "hours" => 3600.0,
        "day" | "days" => 86400.0,
        "week" | "weeks" => 7.0 * 86400.0,
        _ => return None,
    };
    Some(n * unit)
}

// ------------------------------------------------------------------ scontrol

static RE_KV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([A-Za-z_:/]+)=").unwrap());

/// `scontrol show job` output -> {Key: Value}. Values run until the next
/// ` Key=` on the same line, so values with spaces survive.
pub fn parse_scontrol(text: &str) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for line in text.lines() {
        let line = line.trim();
        let keys: Vec<(usize, usize, &str)> = RE_KV
            .captures_iter(line)
            .map(|c| (c.get(0).unwrap().start(), c.get(0).unwrap().end(), c.get(1).unwrap().as_str()))
            .filter(|(start, _, _)| *start == 0 || line[..*start].ends_with(' ')) // an '=' inside a value is not a key
            .collect();
        for (i, (_, end, key)) in keys.iter().enumerate() {
            let value_end = keys.get(i + 1).map(|k| k.0).unwrap_or(line.len());
            let value = line[*end..value_end].trim();
            out.insert(key.to_string(), value.to_string());
        }
    }
    out
}

pub fn scontrol_job(tools: &Tools, jobid: &str) -> Result<IndexMap<String, String>, String> {
    let out = run("scontrol", &tools.scontrol, &["show", "job", jobid])?;
    let kv = parse_scontrol(&out);
    if kv.is_empty() {
        return Err(format!("scontrol: no details for job {}", jobid));
    }
    Ok(kv)
}

/// The script the job was submitted with (only while Slurm still holds the job).
pub fn scontrol_script(tools: &Tools, jobid: &str) -> Result<String, String> {
    let out = run("scontrol", &tools.scontrol, &["write", "batch_script", jobid, "-"])?;
    if out.trim().is_empty() {
        return Err(format!("scontrol: no script for job {} (finished, or not a batch job)", jobid));
    }
    Ok(out)
}

/// Tasks in an array spec: `[1-3]` -> 3, `[1-100%5]` -> 100, `[2,5,7-9]` -> 5.
pub fn array_task_count(spec: &str) -> usize {
    let inner = spec.trim().trim_start_matches('[').trim_end_matches(']');
    let inner = inner.split('%').next().unwrap_or("");
    inner
        .split(',')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('-') {
            Some((a, b)) => match (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                (Ok(a), Ok(b)) if b >= a => b - a + 1,
                _ => 1,
            },
            None => 1,
        })
        .sum()
}

pub fn scancel(tools: &Tools, jobid: &str) -> Result<String, String> {
    run("scancel", &tools.scancel, &[jobid])?;
    Ok(format!("scancel {}: sent", jobid))
}

/// Who am I, for `-u`.
pub fn current_user() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "unknown".to_string())
}

/// The cluster's name, if Slurm tells us, else the host name.
pub fn cluster_name() -> String {
    if let Ok(c) = std::env::var("SLURM_CLUSTER_NAME") {
        if !c.is_empty() {
            return c;
        }
    }
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::process::Command::new("hostname").arg("-s").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "cluster".to_string())
}
