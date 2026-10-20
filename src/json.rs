//! `--json`: queue, live stats, history and efficiency as one document.

use serde_json::{json, Value};

use hisstop::clock;

use crate::model::{cpu_pct, efficiency_by_name, maxrss, mem_pct, summary};
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
    let jobs = mon.jobs();
    let hist = mon.history();
    let s = summary(&jobs, &hist);
    let queue: Vec<Value> = jobs
        .iter()
        .map(|j| {
            let stat = mon.stat_for(&j.jobid);
            let h = mon.hist_for(&j.jobid);
            json!({
                "jobid": j.jobid, "name": j.name, "state": j.state, "partition": j.partition,
                "time_used": opt_i(j.time_used), "time_limit": opt_i(j.time_limit), "time_use": opt_f(j.time_use()),
                "cpus": j.cpus, "mem_req": opt_u(j.mem_req), "nodes": j.nodes, "nodelist": j.nodelist, "reason": j.reason,
                "submit": opt_f(j.submit), "start": opt_f(j.start), "deadline": opt_f(j.deadline()),
                "priority": j.priority, "account": j.account, "qos": j.qos, "workdir": j.workdir,
                "array_job": j.array_job, "array_task": j.array_task, "dependency": j.dependency,
                "cpu_secs": opt_f(crate::model::cpu_secs(stat.as_ref(), h.as_ref())),
                "cpu_pct": opt_f(cpu_pct(j, stat.as_ref(), h.as_ref())),
                "max_rss": opt_u(maxrss(stat.as_ref(), h.as_ref())),
                "mem_pct": opt_f(mem_pct(j, stat.as_ref(), h.as_ref())),
                "details": mon.details_for(&j.jobid).map(|kv| Value::Object(kv.into_iter().map(|(k, v)| (k, Value::from(v))).collect())).unwrap_or(Value::Null),
            })
        })
        .collect();
    let finished: Vec<Value> = hist
        .iter()
        .map(|h| {
            let r = &h.row;
            json!({
                "jobid": r.jobid, "name": h.name, "state": r.state, "short": r.short(), "exit_code": r.exit_code, "signal": r.signal,
                "elapsed": opt_i(r.elapsed), "timelimit": opt_i(r.timelimit), "time_use": opt_f(r.time_use()),
                "req_mem": opt_u(r.req_mem), "max_rss": opt_u(r.max_rss), "mem_use": opt_f(r.mem_use()),
                "total_cpu": opt_f(r.total_cpu), "alloc_cpus": opt_i(r.alloc_cpus), "cpu_eff": opt_f(r.cpu_eff()),
                "nodelist": r.nodelist, "partition": h.partition,
                "submit": opt_f(h.submit), "start": opt_f(h.start), "end": opt_f(h.end), "wait": opt_f(h.wait()),
                "bad": r.is_bad(),
            })
        })
        .collect();
    let efficiency: Vec<Value> = efficiency_by_name(&hist)
        .iter()
        .map(|e| {
            let (note, level) = e.note();
            json!({
                "name": e.rule, "jobs": e.jobs, "elapsed_max": opt_i(e.elapsed_max), "limit": opt_i(e.limit),
                "time_use_max": opt_f(e.time_use_max), "rss_max": opt_u(e.rss_max), "req_mem": opt_u(e.req_mem),
                "mem_use_max": opt_f(e.mem_use_max), "cpu_eff_median": opt_f(e.cpu_eff_median),
                "oom": e.oom, "timeout": e.timeout, "node_fail": e.node_fail, "failed": e.failed, "cancelled": e.cancelled,
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
