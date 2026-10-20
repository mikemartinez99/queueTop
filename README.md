# queuetop

`htop` for your Slurm queue. Where [hisstop](https://github.com/mikemartinez99/hisstop) follows one
Snakemake run, queuetop follows everything you have in Slurm: your running
and pending jobs with their live CPU and memory use, the jobs that finished
recently with why they ended, and how efficient they were. One static
binary, nothing to install on the cluster.

```
queuetop   mike@discovery   3 running   2 pending   16 cpus, 359.0G in use   next start in 58:12   5 finished, 3 badly
squeue 2s ago · sstat 11s ago · sacct 40s ago (last 24hours)

QUEUE   5
jobid           name             partition     state     time      limit     cpus   cpu%   mem req   maxrss   mem%   node / reason
9849151         trimming.2       standard      RUNNING   51:00     2:00:00   4      60%    58.0G     3.0G     5%     t03
9849161         ribodetector.6   bigmem        RUNNING   32:00     3:00:00   10     0%     293.0G    50.0G    17%    s09
9849180         jupyter          interactive   RUNNING   1:02:03   8:00:00   2      10%    8.0G      4.0G     50%    q01
9849170         alignment.8      standard      PENDING   0:00      2:00:00   8      -      32.0G     -        -      (Priority)
9849171_[1-3]   array_task       standard      PENDING   0:00      1:00:00   1      -      4.0G      -        -      (Resources)

FINISHED   5   f to move through them
jobid     name           state           elapsed   limit     maxrss   req     mem%   cpu%   ended
9849101   alignment.7    OUT_OF_MEMORY   40:00     2:00:00   32.0G    32.0G   100%   47%    Oct 20 16:45:00
9849102   qc_report      TIMEOUT         1:00:05   1:00:00   500M     8.0G    6%     98%    Oct 20 16:02:05
...
```

## Install

Same as hisstop, same script:

```bash
scripts/build-linux.sh x86_64 mycluster:bin/     # from the repo root on your laptop
ssh mycluster 'chmod +x ~/bin/queuetop && ~/bin/queuetop --version'
```

On the login node:

```bash
queuetop
```

## What it shows

| Area | Source | Meaning |
|---|---|---|
| **Header** | `squeue`, `sacct` | running / pending counts, cores and memory you hold, Slurm's estimate for your next start, finished jobs in the window and how many ended badly |
| **QUEUE** | `squeue -u you` | every job of yours; `time` against `limit`, `cpu%` and `mem%` are live from `sstat` while the job runs |
| **FINISHED** | `sacct -u you -S now-24hours` | jobs that ended in the window: why (`COMPLETED`, `OUT_OF_MEMORY`, `TIMEOUT`, `exit 1`, `CANCELLED` ...), elapsed against the limit, MaxRSS against the request, CPU efficiency |
| **Notice line** | | a job of yours just completed or ended badly (20 s) |

`cpu%` is CPU seconds consumed divided by elapsed time times cores: 100% means
every core you asked for was busy the whole time. `mem%` is the largest
resident set so far against what you requested. Colours: green running,
yellow pending, red ended badly, dim completed.

## Keys

| Key | Does |
|---|---|
| `j` `k` `↑` `↓` | move the cursor (`J` `K` `PgUp` `PgDn` by page) |
| `Enter` | open the job under the cursor |
| `f` | move through the finished jobs instead of the queue |
| `/` | filter by jobid, name, partition, state, node or reason; `Esc` clears |
| `s` | sort the queue: state, time, cpu%, mem%, name, priority |
| `e` | **efficiency**: finished jobs grouped by name with a verdict per name |
| `x` | `scancel` the job under the cursor, after `y`/`n` (`--no-cancel` disables it) |
| `r` | poll everything now |
| `?` | help · `q` quit |

**The job popup** (`Enter`) combines `squeue`, `sstat`, `sacct` and
`scontrol show job`: state and reason (with Slurm's expected start for a
pending job), partition / account / QOS / node, cores and memory requested,
time used against the limit and when the limit hits, live CPU busy-ness and
MaxRSS, submit and start times with the queue wait, dependencies, array
membership, priority, the command and working directory. Below that it tails
the job's output file (`StdOut`, `l` switches to `StdErr`) with trouble lines
in red. `j` `k` scroll, `G` follows the end, `Esc` closes.

**The efficiency view** (`e`) groups finished jobs by name, the way an array
or a pipeline rule produces many alike, and ends each row with a verdict:
`OOM x2: raise mem`, `TIMEOUT x1: raise runtime`, `near time limit`,
`mem over-requested: peak 500M of 8.0G`, or `ok`. That is what to put in your
next `sbatch --mem` and `--time`.

## Options

```
queuetop [options]
```

| Option | Default | Meaning |
|---|---|---|
| `-u, --user NAME` | you | whose jobs |
| `--since WHEN` | `now-24hours` | how far back `sacct` looks (its `-S` syntax: `now-7days`, `2026-10-01`) |
| `--interval S` | 2 | screen refresh |
| `--squeue-interval S` | 5 | queue polling |
| `--sstat-interval S` / `--no-sstat` | 15 | live usage polling |
| `--sacct-interval S` / `--no-sacct` | 60 | finished-jobs polling |
| `--no-cancel` | | disable `x` |
| `--bell` | | ring when a job of yours ends |
| `--on-event CMD` | | run a shell command when a job ends (`HISSTOP_EVENT`, `HISSTOP_JOBID`, `HISSTOP_RULE` = job name, `HISSTOP_MESSAGE`) |
| `--once` | | print a text snapshot and exit (also when stdout is not a terminal) |
| `--json` | | print everything as JSON and exit |

Every Slurm command runs in a background thread with a timeout, so a slow
controller never freezes the screen. `sstat` only answers for your own
running jobs and only once a step is running; `sacct` needs accounting to be
enabled on the cluster. Both switch themselves off when missing.

## Development

```bash
cargo test            # parsers, the monitor against fake Slurm tools, screen renders, --once/--json
cargo clippy --all-targets
```

queuetop depends on the [hisstop](https://github.com/mikemartinez99/hisstop)
crate from GitHub (`main` branch, see `Cargo.toml`) and reuses its library:
`sacct` units and verdicts, tables, the popup machinery, the key handling.
`cargo update -p hisstop` picks up a newer hisstop. `tests/fake/` holds stand-ins for `squeue`, `sstat`, `sacct`,
`scontrol` and `scancel` with canned output. Layout: `src/slurm.rs`
(commands and parsers), `src/monitor.rs` (pollers), `src/model.rs` (derived
numbers), `src/text.rs` (tables, `--once`), `src/ui.rs`, `src/app.rs`,
`src/json.rs`.

## AI disclosure

Like hisstop, queuetop was written by Claude (Anthropic) in sessions directed
by Mike Martinez. It has been tested against the fake Slurm tools in
`tests/fake/` and not yet against a real cluster; the `squeue` and `sacct`
formats follow the Slurm documentation, but field spellings vary between
Slurm versions, so report what looks wrong.
