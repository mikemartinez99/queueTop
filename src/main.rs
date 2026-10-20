use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;

use hisstop::events::Hooks;
use queuetop::app::{run, App};
use queuetop::monitor::{Monitor, Options};
use queuetop::slurm::{cluster_name, current_user, Tools};
use queuetop::text::snapshot_text;

/// queuetop - htop for your Slurm queue.
///
/// Your running and pending jobs with their live CPU and memory use, the jobs
/// that finished recently with why they ended, and how efficient they were.
#[derive(Parser, Debug)]
#[command(name = "queuetop", version, about, long_about = None)]
struct Cli {
    /// Slurm user to watch (default: you)
    #[arg(short, long)]
    user: Option<String>,

    /// how far back sacct looks for finished jobs (sacct -S syntax)
    #[arg(long, default_value = "now-24hours")]
    since: String,

    /// screen refresh seconds
    #[arg(long, default_value_t = 2.0)]
    interval: f64,

    /// seconds between squeue polls
    #[arg(long = "squeue-interval", default_value_t = 5.0)]
    squeue_interval: f64,

    /// seconds between sstat polls (live CPU and memory of running jobs)
    #[arg(long = "sstat-interval", default_value_t = 15.0)]
    sstat_interval: f64,

    /// seconds between sacct polls (finished jobs)
    #[arg(long = "sacct-interval", default_value_t = 60.0)]
    sacct_interval: f64,

    /// never call sstat
    #[arg(long = "no-sstat")]
    no_sstat: bool,

    /// never call sacct
    #[arg(long = "no-sacct")]
    no_sacct: bool,

    /// disable the x key (scancel)
    #[arg(long = "no-cancel")]
    no_cancel: bool,

    /// ring the terminal bell when one of your jobs ends badly or completes
    #[arg(long)]
    bell: bool,

    /// run this shell command when a job ends (HISSTOP_EVENT, HISSTOP_JOBID, HISSTOP_RULE=job name ...)
    #[arg(long = "on-event", value_name = "CMD")]
    on_event: Option<String>,

    /// print a plain-text snapshot and exit
    #[arg(long)]
    once: bool,

    /// print everything as JSON and exit
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let user = cli.user.clone().unwrap_or_else(current_user);
    let opts = Options {
        squeue_interval: cli.squeue_interval,
        sstat_interval: cli.sstat_interval,
        sacct_interval: cli.sacct_interval,
        sstat: !cli.no_sstat,
        sacct: !cli.no_sacct,
        since: cli.since.clone(),
    };
    let mut mon = Monitor::new(&user, &cluster_name(), Tools::default(), opts);
    if cli.json {
        mon.wait_all(Some(60.0));
        println!("{}", queuetop::json::snapshot_json_string(&mon));
        return ExitCode::SUCCESS;
    }
    if cli.once || !std::io::stdout().is_terminal() {
        mon.wait_all(Some(60.0));
        println!("{}", snapshot_text(&mon, None));
        return ExitCode::SUCCESS;
    }
    let hooks = Hooks { bell: cli.bell, command: cli.on_event.clone() };
    match run(App::new(mon, hooks, !cli.no_cancel), cli.interval) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("queuetop: {}", e);
            ExitCode::from(1)
        }
    }
}
