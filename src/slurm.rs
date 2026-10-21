//! The Slurm commands queuetop runs and the parsers for their output.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{Local, NaiveDateTime, TimeZone};
use indexmap::IndexMap;
use regex::Regex;

use hisstop::sacct::{parse_cpu_time, parse_limit, parse_mem, parse_req_mem, SacctRow};
use hisstop::slurm::{find_exe, parse_slurm_elapsed, run_with_timeout};

const TIMEOUT: Duration = Duration::from_secs(45);

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

fn run(name: &str, over: &Option<PathBuf>, args: &[&str]) -> Result<String, String> {
    let exe = over.clone().or_else(|| find_exe(name)).ok_or_else(|| format!("{} not found", name))?;
    let (out, err, ok) = run_with_timeout(&exe, args, TIMEOUT).map_err(|e| format!("{}: {}", name, e))?;
    if !ok {
        let last = err.trim().lines().last().unwrap_or("failed");
        return Err(format!("{}: {}", name, last.chars().take(80).collect::<String>()));
    }
    Ok(out)
}

/// `2026-10-20T17:05:49` -> local epoch; `N/A`, `Unknown`, `` -> None.
pub fn parse_slurm_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok()?;
    Local.from_local_datetime(&naive).earliest().map(|t| t.timestamp() as f64)
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
}

impl QueueJob {
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
            time_limit: parse_limit(g(5)),
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
        let step_cpu = f
            .get(5)
            .and_then(|t| tres_cpu_seconds(t))
            .or_else(|| sane_cpu_time(f[1]).map(|c| c * ntasks.clamp(1.0, 100_000.0)));
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

/// A job as the accounting database remembers it (steps folded in).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistJob {
    pub row: SacctRow,
    pub name: String,
    pub partition: String,
    pub submit: Option<f64>,
    pub start: Option<f64>,
    pub end: Option<f64>,
}

impl HistJob {
    pub fn jobid(&self) -> &str {
        &self.row.jobid
    }

    /// Queue wait in seconds.
    pub fn wait(&self) -> Option<f64> {
        Some(self.start? - self.submit?)
    }
}

fn normalise_state(s: &str) -> String {
    s.split_whitespace().next().unwrap_or("").trim_end_matches('+').to_uppercase()
}

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
        let h = out.entry(base.clone()).or_insert_with(|| HistJob { row: SacctRow { jobid: base.clone(), ..Default::default() }, ..Default::default() });
        if let Some(r) = parse_mem(f[7]).filter(|v| *v > 0) {
            h.row.max_rss = Some(h.row.max_rss.map_or(r, |c| c.max(r)));
        }
        if is_step {
            continue;
        }
        h.name = f[1].to_string();
        h.row.state = normalise_state(f[2]);
        if let Some((c, s)) = f[3].split_once(':') {
            h.row.exit_code = c.trim().parse().ok();
            h.row.signal = s.trim().parse().ok();
        }
        h.row.elapsed = parse_slurm_elapsed(f[4]);
        h.row.timelimit = parse_limit(f[5]);
        h.row.alloc_cpus = f[9].trim().parse().ok();
        h.row.req_mem = parse_req_mem(f[6], h.row.alloc_cpus);
        h.row.total_cpu = sane_cpu_time(f[8]);
        h.row.nodelist = f[10].trim().to_string();
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
