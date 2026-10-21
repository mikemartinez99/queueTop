//! Derived numbers: per-job CPU and memory use, the queue summary, array
//! grouping, and the per-name efficiency aggregation with its verdicts and
//! suggested next requests.

use indexmap::IndexMap;

use crate::slurm::{array_task_count, HistJob, QueueJob, Stat};
use crate::text::{fmt_dur, fmt_mem, pct};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub running: usize,
    pub pending: usize,
    pub other: usize,
    pub cpus_running: i64,
    pub mem_running: u64,
    /// Slurm's earliest estimated start among pending jobs
    pub next_start: Option<f64>,
    pub finished: usize,
    pub finished_bad: usize,
}

pub fn summary(jobs: &[QueueJob], hist: &[HistJob]) -> Summary {
    let mut s = Summary::default();
    for j in jobs {
        if j.is_running() {
            s.running += 1;
            s.cpus_running += j.cpus;
            s.mem_running += j.mem_req.unwrap_or(0);
        } else if j.is_pending() {
            s.pending += 1;
            if let Some(t) = j.start {
                s.next_start = Some(s.next_start.map_or(t, |c: f64| c.min(t)));
            }
        } else {
            s.other += 1;
        }
    }
    for h in hist {
        if h.is_final() {
            s.finished += 1;
            if h.is_bad() {
                s.finished_bad += 1;
            }
        }
    }
    s
}

/// CPU seconds used so far. For a running job only sstat counts: sacct's
/// TotalCPU is usually written when the job ends and reads 0 until then, which
/// would show as a misleading 0%. A reading beyond what the allocation could
/// have consumed (every core busy for the whole elapsed time, with a wide
/// margin) is treated as unknown.
pub fn cpu_secs(job: &QueueJob, stat: Option<&Stat>, hist: Option<&HistJob>) -> Option<f64> {
    let raw = if job.is_running() { stat.and_then(|s| s.cpu_secs)? } else { hist.and_then(|h| h.total_cpu).filter(|c| *c > 0.0)? };
    let cap = job.time_used.unwrap_or(0) as f64 * job.cpus.max(1) as f64 * 4.0 + 3600.0;
    if raw > cap {
        return None;
    }
    Some(raw)
}

/// Fraction of the allocated cores kept busy so far.
pub fn cpu_pct(job: &QueueJob, stat: Option<&Stat>, hist: Option<&HistJob>) -> Option<f64> {
    let used = job.time_used? as f64;
    if used <= 0.0 || job.cpus <= 0 {
        return None;
    }
    Some(cpu_secs(job, stat, hist)? / (used * job.cpus as f64))
}

pub fn maxrss(stat: Option<&Stat>, hist: Option<&HistJob>) -> Option<u64> {
    stat.and_then(|s| s.max_rss).or_else(|| hist.and_then(|h| h.max_rss))
}

pub fn mem_pct(job: &QueueJob, stat: Option<&Stat>, hist: Option<&HistJob>) -> Option<f64> {
    let req = job.mem_req.filter(|r| *r > 0)?;
    Some(maxrss(stat, hist)? as f64 / req as f64)
}

/// Colour name for a Slurm state (queue or history).
pub fn state_colour(state: &str) -> &'static str {
    match state {
        "RUNNING" | "COMPLETING" | "CONFIGURING" => "running",
        "PENDING" | "SUSPENDED" | "REQUEUED" => "pending",
        "COMPLETED" => "done",
        "FAILED" | "TIMEOUT" | "OUT_OF_MEMORY" | "NODE_FAIL" | "CANCELLED" | "PREEMPTED" | "DEADLINE" | "BOOT_FAIL" => "failed",
        _ => "dim",
    }
}

// ------------------------------------------------------------------ arrays

/// Fold the tasks of each array with two or more rows into one row, placed
/// where its first task was. The row's jobid is `<array>_*`, its cpus and
/// memory are the sums, its time the longest, and `reason` says how many
/// tasks are running, pending and otherwise.
pub fn group_arrays(jobs: Vec<QueueJob>) -> Vec<QueueJob> {
    let mut counts: IndexMap<String, usize> = IndexMap::new();
    for j in &jobs {
        if !j.array_job.is_empty() && !j.array_task.is_empty() && j.array_task != "N/A" {
            *counts.entry(j.array_job.clone()).or_default() += 1;
        }
    }
    let mut out: Vec<QueueJob> = Vec::new();
    let mut groups: IndexMap<String, usize> = IndexMap::new(); // array -> index in out
    for j in jobs {
        let grouped = counts.get(&j.array_job).is_some_and(|n| *n > 1) && !j.array_task.is_empty() && j.array_task != "N/A";
        if !grouped {
            out.push(j);
            continue;
        }
        let idx = *groups.entry(j.array_job.clone()).or_insert_with(|| {
            out.push(QueueJob { jobid: format!("{}_*", j.array_job), array_task: "*".to_string(), time_used: None, cpus: 0, mem_req: None, nodes: 0, nodelist: String::new(), submit: None, start: None, ..j.clone() });
            out.len() - 1
        });
        let g = &mut out[idx];
        g.cpus += j.cpus;
        g.nodes += j.nodes;
        if let Some(m) = j.mem_req {
            g.mem_req = Some(g.mem_req.unwrap_or(0) + m);
        }
        g.time_used = g.time_used.max(j.time_used);
        g.priority = g.priority.max(j.priority);
        g.submit = match (g.submit, j.submit) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if j.is_running() {
            g.start = match (g.start, j.start) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        g.tasks.push(j);
    }
    for g in out.iter_mut().filter(|g| g.is_group()) {
        let (mut running, mut pending, mut other) = (0, 0, 0);
        for t in &g.tasks {
            let n = if t.array_task.starts_with('[') { array_task_count(&t.array_task) } else { 1 };
            if t.is_running() {
                running += n;
            } else if t.is_pending() {
                pending += n;
            } else {
                other += n;
            }
        }
        g.state = if running > 0 { "RUNNING" } else if pending > 0 { "PENDING" } else { "SUSPENDED" }.to_string();
        let mut parts = Vec::new();
        if running > 0 {
            parts.push(format!("{} running", running));
        }
        if pending > 0 {
            parts.push(format!("{} pending", pending));
        }
        if other > 0 {
            parts.push(format!("{} other", other));
        }
        g.reason = parts.join(", ");
        g.name = g.tasks[0].name.clone();
    }
    out
}

// ------------------------------------------------------------------ efficiency

/// Finished jobs of one name (an array, a pipeline rule) and how they fared.
#[derive(Clone, Debug, Default)]
pub struct NameEff {
    pub name: String,
    pub jobs: usize,
    pub elapsed_max: Option<i64>,
    pub limit: Option<i64>,
    pub time_use_max: Option<f64>,
    pub rss_max: Option<u64>,
    pub req_mem: Option<u64>,
    pub mem_use_max: Option<f64>,
    pub cpu_eff_median: Option<f64>,
    pub oom: usize,
    pub timeout: usize,
    pub node_fail: usize,
    pub failed: usize,
    pub cancelled: usize,
}

/// `peak` plus a quarter, rounded up to a round number of bytes.
pub fn suggest_mem(peak: u64) -> u64 {
    const M: u64 = 1024 * 1024;
    const G: u64 = 1024 * M;
    let want = peak + peak / 4;
    let step = if want >= 8 * G { G } else if want >= G { 256 * M } else { 64 * M };
    (want.div_ceil(step) * step).max(128 * M)
}

/// `longest` plus a quarter, rounded up to a quarter hour.
pub fn suggest_time(longest: i64) -> i64 {
    let want = longest + longest / 4;
    ((want + 899) / 900 * 900).max(900)
}

/// Bytes as an sbatch `--mem` value: `12G`, `768M`.
pub fn fmt_slurm_mem(bytes: u64) -> String {
    const M: u64 = 1024 * 1024;
    const G: u64 = 1024 * M;
    if bytes.is_multiple_of(G) {
        format!("{}G", bytes / G)
    } else {
        format!("{}M", bytes.div_ceil(M))
    }
}

impl NameEff {
    fn add(&mut self, h: &HistJob) {
        self.jobs += 1;
        match h.state.as_str() {
            "OUT_OF_MEMORY" => self.oom += 1,
            "TIMEOUT" => self.timeout += 1,
            "NODE_FAIL" | "BOOT_FAIL" => self.node_fail += 1,
            "FAILED" => self.failed += 1,
            "CANCELLED" | "PREEMPTED" | "DEADLINE" | "REVOKED" => self.cancelled += 1,
            _ => {}
        }
        self.elapsed_max = self.elapsed_max.max(h.elapsed);
        self.limit = self.limit.max(h.timelimit);
        self.rss_max = self.rss_max.max(h.max_rss);
        self.req_mem = self.req_mem.max(h.req_mem);
        let max_f = |cur: Option<f64>, v: Option<f64>| v.map(|v| cur.map_or(v, |c| c.max(v))).or(cur);
        self.time_use_max = max_f(self.time_use_max, h.time_use());
        self.mem_use_max = max_f(self.mem_use_max, h.mem_use());
    }

    /// (text, colour name) for the note column.
    pub fn note(&self) -> (String, &'static str) {
        let mut bad: Vec<String> = Vec::new();
        if self.oom > 0 {
            bad.push(format!("OOM x{}: raise mem", self.oom));
        }
        if self.timeout > 0 {
            bad.push(format!("TIMEOUT x{}: raise runtime", self.timeout));
        }
        if self.node_fail > 0 {
            bad.push(format!("NODE_FAIL x{}", self.node_fail));
        }
        if self.failed > 0 {
            bad.push(format!("failed x{}", self.failed));
        }
        if self.cancelled > 0 {
            bad.push(format!("cancelled x{}", self.cancelled));
        }
        if !bad.is_empty() {
            return (bad.join(", "), "failed");
        }
        let mut warn: Vec<String> = Vec::new();
        if self.time_use_max.is_some_and(|u| u > 0.9) {
            warn.push("near time limit".to_string());
        }
        if let Some(u) = self.mem_use_max {
            if u > 0.9 {
                warn.push("near mem limit".to_string());
            } else if u < 0.25 && self.jobs >= 2 {
                if let (Some(r), Some(q)) = (self.rss_max, self.req_mem) {
                    warn.push(format!("mem over-requested: peak {} of {}", fmt_mem(r), fmt_mem(q)));
                }
            }
        }
        if !warn.is_empty() {
            return (warn.join(", "), "pending");
        }
        ("ok".to_string(), "done")
    }

    /// What to ask for next time, from the peaks seen: `--mem=12G --time=1:30:00`.
    /// After an OOM or TIMEOUT the peak is the old limit, so the suggestion
    /// is a quarter above it; raise further if that still fails.
    pub fn next_request(&self) -> String {
        let mut parts = Vec::new();
        if let Some(r) = self.rss_max {
            parts.push(format!("--mem={}", fmt_slurm_mem(suggest_mem(r))));
        }
        if let Some(e) = self.elapsed_max {
            parts.push(format!("--time={}", fmt_dur(Some(suggest_time(e) as f64))));
        }
        parts.join(" ")
    }
}

/// Finished jobs grouped by job name, in order of first appearance.
pub fn efficiency_by_name(hist: &[HistJob]) -> Vec<NameEff> {
    let mut groups: IndexMap<String, (NameEff, Vec<f64>)> = IndexMap::new();
    for h in hist.iter().filter(|h| h.is_final()) {
        let (eff, cpu) = groups.entry(h.name.clone()).or_insert_with(|| (NameEff { name: h.name.clone(), ..Default::default() }, Vec::new()));
        eff.add(h);
        if let Some(c) = h.cpu_eff() {
            cpu.push(c);
        }
    }
    groups
        .into_values()
        .map(|(mut eff, mut cpu)| {
            cpu.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            eff.cpu_eff_median = if cpu.is_empty() { None } else { Some(cpu[cpu.len() / 2]) };
            eff
        })
        .collect()
}

pub fn efficiency_rows(effs: &[NameEff]) -> Vec<Vec<String>> {
    effs.iter()
        .map(|e| {
            vec![
                e.name.clone(),
                e.jobs.to_string(),
                fmt_dur(e.elapsed_max.map(|s| s as f64)),
                fmt_dur(e.limit.map(|s| s as f64)),
                pct(e.time_use_max),
                e.rss_max.map(fmt_mem).unwrap_or_else(|| "-".to_string()),
                e.req_mem.map(fmt_mem).unwrap_or_else(|| "-".to_string()),
                pct(e.mem_use_max),
                pct(e.cpu_eff_median),
                e.next_request(),
                e.note().0,
            ]
        })
        .collect()
}
