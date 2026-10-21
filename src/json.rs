//! `--json`: queue, live stats, history and efficiency as one document.

use serde_json::{json, Value};

use crate::clock;
use crate::model::{cpu_pct, cpu_secs, efficiency_by_name, maxrss, mem_pct, suggest_mem, suggest_time, summary};
use crate::monitor::Monitor;
use crate::VERSION;

fn opt_f(v: Option<f64>) -> Value {
    v.map(Value::from).unwrap_or(Value::Null)
}

fn opt_i(v: Option<i64>) -> Value {
    v.map(Value::from).unwrap_or(Value::Null)
}

fn opt_u(v: Option<u64>) -> Value {
    v.map(Value::from).unwrap_or(Value::Null)
}

pub fn snapshot_json(mon: &Monitor) -> Value {
    let snap = mon.snapshot();
    let s = summary(&snap.jobs, &snap.hist);
    let queue: Vec<Value> = snap
        .jobs
        .iter()
        .map(|j| {
            let (stat, h) = (snap.stat_for(&j.jobid), snap.hist_for(&j.jobid));
            json!({
                "jobid": j.jobid, "name": j.name, "state": j.state, "partition": j.partition,
                "time_used": opt_i(j.time_used), "time_limit": opt_i(j.time_limit), "time_use": opt_f(j.time_use()),
                "cpus": j.cpus, "mem_req": opt_u(j.mem_req), "nodes": j.nodes, "nodelist": j.nodelist, "reason": j.reason,
                "submit": opt_f(j.submit), "start": opt_f(j.start), "deadline": opt_f(j.deadline()),
                "priority": j.priority, "account": j.account, "qos": j.qos, "workdir": j.workdir,
                "array_job": j.array_job, "array_task": j.array_task, "dependency": j.dependency,
                "cpu_secs": opt_f(cpu_secs(j, stat, h)),
                "cpu_pct": opt_f(cpu_pct(j, stat, h)),
                "max_rss": opt_u(maxrss(stat, h)),
                "mem_pct": opt_f(mem_pct(j, stat, h)),
                "details": mon.details_for(&j.jobid).map(|kv| Value::Object(kv.into_iter().map(|(k, v)| (k, Value::from(v))).collect())).unwrap_or(Value::Null),
            })
        })
        .collect();
    let finished: Vec<Value> = snap
        .hist
        .iter()
        .map(|h| {
            json!({
                "jobid": h.jobid, "name": h.name, "state": h.state, "short": h.short(), "exit_code": h.exit_code, "signal": h.signal,
                "elapsed": opt_i(h.elapsed), "timelimit": opt_i(h.timelimit), "time_use": opt_f(h.time_use()),
                "req_mem": opt_u(h.req_mem), "max_rss": opt_u(h.max_rss), "mem_use": opt_f(h.mem_use()),
                "total_cpu": opt_f(h.total_cpu), "alloc_cpus": opt_i(h.alloc_cpus), "cpu_eff": opt_f(h.cpu_eff()),
                "nodelist": h.nodelist, "partition": h.partition,
                "submit": opt_f(h.submit), "start": opt_f(h.start), "end": opt_f(h.end), "wait": opt_f(h.wait()),
                "bad": h.is_bad(), "oom": h.is_oom(),
            })
        })
        .collect();
    let efficiency: Vec<Value> = efficiency_by_name(&snap.hist)
        .iter()
        .map(|e| {
            let (note, level) = e.note();
            json!({
                "name": e.name, "jobs": e.jobs, "elapsed_max": opt_i(e.elapsed_max), "limit": opt_i(e.limit),
                "time_use_max": opt_f(e.time_use_max), "rss_max": opt_u(e.rss_max), "req_mem": opt_u(e.req_mem),
                "mem_use_max": opt_f(e.mem_use_max), "cpu_eff_median": opt_f(e.cpu_eff_median),
                "oom": e.oom, "timeout": e.timeout, "node_fail": e.node_fail, "failed": e.failed, "cancelled": e.cancelled,
                "next_mem": opt_u(e.rss_max.map(suggest_mem)), "next_time": opt_i(e.elapsed_max.map(suggest_time)),
                "next_request": e.next_request(),
                "note": note, "level": level,
            })
        })
        .collect();
    let sh = mon.shared();
    json!({
        "queuetop": VERSION,
        "generated": clock::now(),
        "user": mon.user,
        "cluster": mon.cluster,
        "summary": {
            "running": s.running, "pending": s.pending, "other": s.other,
            "cpus_running": s.cpus_running, "mem_running": s.mem_running, "next_start": opt_f(s.next_start),
            "finished": s.finished, "finished_bad": s.finished_bad,
        },
        "queue": queue,
        "finished": finished,
        "efficiency": efficiency,
        "sources": {
            "squeue": {"polled": opt_f(sh.jobs_time), "error": sh.jobs_err},
            "sstat": {"enabled": sh.use_sstat, "polled": opt_f(sh.stats_time), "error": sh.stats_err},
            "sacct": {"enabled": sh.use_sacct, "polled": opt_f(sh.hist_time), "error": sh.hist_err, "since": mon.opts.since},
        },
    })
}

pub fn snapshot_json_string(mon: &Monitor) -> String {
    serde_json::to_string_pretty(&snapshot_json(mon)).unwrap_or_default()
}
