//! queuetop - htop for your Slurm queue.
//!
//! Where hisstop follows one Snakemake run, queuetop follows everything you
//! have in Slurm: the queue from `squeue`, live CPU and memory use of running
//! jobs from `sstat`, the recent past from `sacct` with exit states and
//! efficiency, and the full details of any job from `scontrol`.

pub mod app;
pub mod clock;
pub mod config;
pub mod events;
pub mod json;
pub mod model;
pub mod monitor;
pub mod slurm;
pub mod tail;
pub mod text;
pub mod ui;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
