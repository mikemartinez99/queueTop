//! Background pollers for squeue, sstat, sacct and scontrol, and the shared
//! state the screens read.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use crate::clock;
use crate::slurm::{jobkey, sacct_user, scancel, scontrol_job, scontrol_script, since_seconds, squeue_user, sstat_jobs, HistJob, QueueJob, Stat, Tools};

#[derive(Clone, Debug)]
pub struct Options {
    pub squeue_interval: f64,
    pub sstat_interval: f64,
    pub sacct_interval: f64,
    pub sstat: bool,
    pub sacct: bool,
    /// sacct -S argument
    pub since: String,
}

impl Default for Options {
    fn default() -> Self {
        Options { squeue_interval: 5.0, sstat_interval: 15.0, sacct_interval: 60.0, sstat: true, sacct: true, since: "now-24hours".to_string() }
    }
}

/// How long scontrol details stay fresh.
const DETAILS_TTL: f64 = 30.0;
/// How long unused scontrol answers stay cached at all.
const DETAILS_KEEP: f64 = 600.0;

#[derive(Debug, Default)]
pub struct Shared {
    pub jobs: Vec<QueueJob>,
    pub jobs_time: Option<f64>,
    pub jobs_err: String,
    pub stats: IndexMap<String, Stat>,
    pub stats_time: Option<f64>,
    pub stats_err: String,
    pub use_sstat: bool,
    pub hist: IndexMap<String, HistJob>,
    pub hist_time: Option<f64>,
    pub hist_err: String,
    pub use_sacct: bool,
    /// jobid -> (fetched at, scontrol key/values)
    pub details: HashMap<String, (f64, IndexMap<String, String>)>,
    pub details_err: HashMap<String, String>,
    /// jobid -> (fetched at, the batch script or why not)
    pub scripts: HashMap<String, (f64, Result<String, String>)>,
}

#[derive(Default)]
struct Poller {
    last: f64,
    thread: Option<JoinHandle<()>>,
}

impl Poller {
    fn busy(&self) -> bool {
        self.thread.as_ref().is_some_and(|h| !h.is_finished())
    }

    fn due(&mut self, now: f64, interval: f64) -> bool {
        if self.busy() || now - self.last < interval {
            return false;
        }
        self.last = now;
        true
    }

    fn spawn<F: FnOnce() + Send + 'static>(&mut self, name: &str, f: F) {
        self.thread = Some(thread::Builder::new().name(name.into()).spawn(f).expect("spawn poller"));
    }

    fn wait(&mut self, timeout: Option<f64>) {
        if let Some(h) = self.thread.take() {
            let start = Instant::now();
            loop {
                if h.is_finished() {
                    let _ = h.join();
                    return;
                }
                if let Some(t) = timeout {
                    if start.elapsed().as_secs_f64() >= t {
                        self.thread = Some(h);
                        return;
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// One consistent copy of everything a screen needs, taken under one lock.
#[derive(Clone, Debug, Default)]
pub struct Snap {
    /// queue jobs: running first, then pending, then the rest; by jobid within
    pub jobs: Vec<QueueJob>,
    /// finished jobs inside the window, most recently ended first
    pub hist: Vec<HistJob>,
    stats: IndexMap<String, Stat>,
    all_hist: IndexMap<String, HistJob>,
}

impl Snap {
    pub fn stat_for(&self, jobid: &str) -> Option<&Stat> {
        self.stats.get(jobid)
    }

    /// The accounting row for a job, finished or not.
    pub fn hist_for(&self, jobid: &str) -> Option<&HistJob> {
        self.all_hist.get(jobid)
    }
}

pub struct Monitor {
    pub user: String,
    pub cluster: String,
    pub tools: Tools,
    pub opts: Options,
    shared: Arc<Mutex<Shared>>,
    squeue: Poller,
    sstat: Poller,
    sacct: Poller,
    /// one-shot scontrol fetches in flight, keyed "job:<id>" or "script:<id>"
    oneshots: HashMap<String, Poller>,
}

impl Monitor {
    pub fn new(user: &str, cluster: &str, tools: Tools, opts: Options) -> Monitor {
        let shared = Shared { use_sstat: opts.sstat, use_sacct: opts.sacct, ..Default::default() };
        let mut m = Monitor {
            user: user.to_string(),
            cluster: cluster.to_string(),
            tools,
            opts,
            shared: Arc::new(Mutex::new(shared)),
            squeue: Poller::default(),
            sstat: Poller::default(),
            sacct: Poller::default(),
            oneshots: HashMap::new(),
        };
        m.refresh();
        m
    }

    pub fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Kick off whichever polls are due.
    pub fn refresh(&mut self) {
        let now = clock::now();
        if self.squeue.due(now, self.opts.squeue_interval) {
            let (shared, tools, user) = (Arc::clone(&self.shared), self.tools.clone(), self.user.clone());
            self.squeue.spawn("squeue", move || {
                let res = squeue_user(&tools, &user);
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                s.jobs_time = Some(clock::now());
                match res {
                    Ok(jobs) => {
                        s.jobs = jobs;
                        s.jobs_err.clear();
                    }
                    Err(e) => s.jobs_err = e,
                }
            });
        }
        let (use_sstat, use_sacct, running): (bool, bool, Vec<String>) = {
            let mut s = self.shared();
            s.details.retain(|_, (t, _)| now - *t < DETAILS_KEEP);
            s.scripts.retain(|_, (t, _)| now - *t < DETAILS_KEEP);
            (s.use_sstat, s.use_sacct, s.jobs.iter().filter(|j| j.is_running()).map(|j| j.jobid.clone()).collect())
        };
        if use_sstat && !running.is_empty() && self.sstat.due(now, self.opts.sstat_interval) {
            let (shared, tools) = (Arc::clone(&self.shared), self.tools.clone());
            self.sstat.spawn("sstat", move || {
                let res = sstat_jobs(&tools, &running);
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                s.stats_time = Some(clock::now());
                match res {
                    Ok(stats) => {
                        s.stats = stats;
                        s.stats_err.clear();
                    }
                    Err(e) => {
                        if e == "sstat not found" {
                            s.use_sstat = false;
                        }
                        s.stats_err = e;
                    }
                }
            });
        }
        if use_sacct && self.sacct.due(now, self.opts.sacct_interval) {
            let (shared, tools, user, since) = (Arc::clone(&self.shared), self.tools.clone(), self.user.clone(), self.opts.since.clone());
            self.sacct.spawn("sacct", move || {
                let res = sacct_user(&tools, &user, &since);
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                s.hist_time = Some(clock::now());
                match res {
                    Ok(h) => {
                        s.hist = h;
                        s.hist_err.clear();
                    }
                    Err(e) => {
                        if e == "sacct not found" {
                            s.use_sacct = false;
                        }
                        s.hist_err = e;
                    }
                }
            });
        }
        self.oneshots.retain(|_, p| p.busy());
    }

    /// Start a one-shot fetch under `key` unless one is running or just ran.
    fn oneshot<F: FnOnce() + Send + 'static>(&mut self, key: String, f: F) {
        let now = clock::now();
        let p = self.oneshots.entry(key).or_default();
        if p.busy() || now - p.last < 2.0 {
            return;
        }
        p.last = now;
        p.spawn("scontrol", f);
    }

    /// Fetch `scontrol show job` for one job unless a fresh copy exists.
    pub fn request_details(&mut self, jobid: &str) {
        let now = clock::now();
        if self.shared().details.get(jobid).is_some_and(|(t, _)| now - t < DETAILS_TTL) {
            return;
        }
        let (shared, tools, id) = (Arc::clone(&self.shared), self.tools.clone(), jobid.to_string());
        self.oneshot(format!("job:{}", jobid), move || {
            let res = scontrol_job(&tools, &id);
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            match res {
                Ok(kv) => {
                    s.details.insert(id.clone(), (clock::now(), kv));
                    s.details_err.remove(&id);
                }
                Err(e) => {
                    s.details_err.insert(id, e);
                }
            }
        });
    }

    /// Fetch the submitted batch script of one job unless a fresh copy exists.
    pub fn request_script(&mut self, jobid: &str) {
        let now = clock::now();
        if self.shared().scripts.get(jobid).is_some_and(|(t, _)| now - t < DETAILS_TTL) {
            return;
        }
        let (shared, tools, id) = (Arc::clone(&self.shared), self.tools.clone(), jobid.to_string());
        self.oneshot(format!("script:{}", jobid), move || {
            let res = scontrol_script(&tools, &id);
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            s.scripts.insert(id, (clock::now(), res));
        });
    }

    pub fn wait_all(&mut self, timeout: Option<f64>) {
        let start = Instant::now();
        let left = |t: Option<f64>| t.map(|t| (t - start.elapsed().as_secs_f64()).max(0.0));
        self.squeue.wait(timeout);
        // sstat / sacct may only become due once squeue has answered
        self.refresh();
        self.sstat.wait(left(timeout));
        self.sacct.wait(left(timeout));
        for p in self.oneshots.values_mut() {
            p.wait(left(timeout));
        }
    }

    // ---- reads

    pub fn snapshot(&self) -> Snap {
        self.snapshot_at(clock::now())
    }

    /// Everything a screen needs as of `now`, under one lock. A finished job
    /// that ended longer ago than the `--since` window is left out, so the
    /// list empties over a long session.
    pub fn snapshot_at(&self, now: f64) -> Snap {
        let keep = since_seconds(&self.opts.since);
        let s = self.shared();
        let mut jobs = s.jobs.clone();
        jobs.sort_by_key(|j| (if j.is_running() { 0 } else if j.is_pending() { 1 } else { 2 }, jobkey(&j.jobid)));
        let mut hist: Vec<HistJob> = s.hist.values().filter(|h| h.is_final() && keep.is_none_or(|k| h.end.is_none_or(|e| now - e <= k))).cloned().collect();
        hist.sort_by(|a, b| b.end.unwrap_or(0.0).partial_cmp(&a.end.unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal).then_with(|| jobkey(&b.jobid).cmp(&jobkey(&a.jobid))));
        Snap { jobs, hist, stats: s.stats.clone(), all_hist: s.hist.clone() }
    }

    pub fn jobs(&self) -> Vec<QueueJob> {
        self.snapshot().jobs
    }

    pub fn history(&self) -> Vec<HistJob> {
        self.snapshot().hist
    }

    pub fn history_at(&self, now: f64) -> Vec<HistJob> {
        self.snapshot_at(now).hist
    }

    pub fn details_for(&self, jobid: &str) -> Option<IndexMap<String, String>> {
        self.shared().details.get(jobid).map(|(_, kv)| kv.clone())
    }

    pub fn details_err(&self, jobid: &str) -> Option<String> {
        self.shared().details_err.get(jobid).cloned()
    }

    pub fn script_for(&self, jobid: &str) -> Option<Result<String, String>> {
        self.shared().scripts.get(jobid).map(|(_, r)| r.clone())
    }

    pub fn cancel(&self, jobid: &str) -> Result<String, String> {
        scancel(&self.tools, jobid)
    }
}
