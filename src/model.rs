//! Derived numbers: per-job CPU and memory use, the queue summary, and the
//! per-name efficiency aggregation (reusing hisstop's verdict logic).

use indexmap::IndexMap;

use hisstop::efficiency::RuleEff;

use crate::slurm::{HistJob, QueueJob, Stat};

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
        if h.row.is_final() {
            s.finished += 1;
            if h.row.is_bad() {
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
    let raw = if job.is_running() {
        stat.and_then(|s| s.cpu_secs)?
    } else {
        hist.and_then(|h| h.row.total_cpu).filter(|c| *c > 0.0)?
    };
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
    stat.and_then(|s| s.max_rss).or_else(|| hist.and_then(|h| h.row.max_rss))
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

/// Finished jobs grouped by job name, with hisstop's verdicts.
pub fn efficiency_by_name(hist: &[HistJob]) -> Vec<RuleEff> {
    let mut groups: IndexMap<String, (RuleEff, Vec<f64>)> = IndexMap::new();
    for h in hist.iter().filter(|h| h.row.is_final()) {
        let (eff, cpu) = groups.entry(h.name.clone()).or_insert_with(|| (RuleEff { rule: h.name.clone(), ..Default::default() }, Vec::new()));
        eff.add(&h.row);
        if let Some(c) = h.row.cpu_eff() {
            cpu.push(c);
        }
    }
    groups
        .into_iter()
        .map(|(_, (mut eff, mut cpu))| {
            cpu.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            eff.cpu_eff_median = if cpu.is_empty() { None } else { Some(cpu[cpu.len() / 2]) };
            eff
        })
        .collect()
}
