<p align="center"><img src="docs/logo.svg" width="128" alt="queuetop logo"></p>

# queuetop

`htop` for your Slurm queue. Every job you have in Slurm, with live CPU and
memory use while it runs, the jobs that finished recently with why they
ended, and how efficient they were, so the next `--mem` and `--time` you ask
for are the right ones. One static binary, nothing to install on the cluster.

![queuetop: the queue with live cpu% and mem%, and the jobs that finished today](docs/screenshot.png)

Press `Enter` on a job for everything Slurm knows about it and a live tail of
its output file. Here a job has just been cancelled with `x`:

![the job popup: command, output path, and the output file tailing live with the Slurm termination line in red](docs/popup.png)

queuetop is the sibling of [hisstop](https://github.com/mikemartinez99/hisstop),
which follows one Snakemake run; queuetop follows your whole queue.

## Installing on an HPC cluster

Login nodes rarely have Rust, so build a static Linux binary on your own
machine and copy it over.

**1. On your laptop, once:** install Rust and clone this repository.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # then open a new shell
git clone https://github.com/mikemartinez99/queuetop.git && cd queuetop
```

**2. Build for the cluster and copy it over.** Use the host name or SSH alias
you normally `ssh` to. The script adds the cross-compile target the first
time, builds a fully static x86_64 binary (about 3 MB) and runs `scp`.

```bash
scripts/build-linux.sh x86_64 mycluster:bin/
```

For an arm64 cluster say `aarch64` instead of `x86_64`. Without the second
argument it only builds; the binary is then at
`target/x86_64-unknown-linux-musl/release/queuetop`. No Docker, zig or
cross-compiler is needed: the musl target ships its own C runtime and the
Rust toolchain links the ELF itself.

**3. On the login node:** make it executable and check that it runs.

```bash
mkdir -p ~/bin && chmod +x ~/bin/queuetop && ~/bin/queuetop --version
```

If `queuetop` is not found afterwards, `~/bin` is not on your PATH there; add
`export PATH="$HOME/bin:$PATH"` to your `~/.bashrc`. The binary runs on any
Linux from kernel 3.10 up (RHEL 7 and newer) with no modules to load.

**4. Use it.** From anywhere on the login node:

```bash
queuetop
```

`q` quits. `squeue` is polled every 5 s, `sstat` every 15 s and `sacct`
every 60 s; each runs in a background thread with a timeout, so a slow
controller never freezes the screen, and each switches itself off if the
command is missing. If accounting is not enabled on your cluster,
`--no-sacct` saves the attempts.

## What you see

| Area | Source | Meaning |
|---|---|---|
| **Header** | `squeue`, `sacct` | running and pending counts, the cores and memory you hold, Slurm's estimate for your next start, finished jobs in the window and how many ended badly |
| **QUEUE** | `squeue -u you` | every job of yours: partition, state, `time` against `limit`, cores, memory requested, node or pending reason; `cpu%` and `mem%` are live from `sstat` while the job runs |
| **FINISHED** | `sacct -u you -S now-24hours` | jobs that ended in the window: why (`COMPLETED`, `OUT_OF_MEMORY`, `TIMEOUT`, `exit 1`, `CANCELLED` ...), elapsed against the limit, MaxRSS against the request, CPU efficiency, end time |
| **Notice line** | | a job of yours just completed or ended badly, for 20 s |

`cpu%` is CPU seconds consumed over elapsed time times cores: 100% means
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
| `?` | help · `q` quit · `Ctrl-C` quit |

**The job popup** (`Enter`) combines `squeue`, `sstat`, `sacct` and
`scontrol show job`: state and reason, with Slurm's expected start for a
pending job; partition, account, QOS and node; cores and memory requested;
time used against the limit and the clock time the limit hits; live CPU
busy-ness and MaxRSS; submit and start times with the queue wait;
dependencies, array membership, priority, the command and the working
directory. Below that it tails the job's output file (`StdOut`, `l` switches
to `StdErr`) with trouble lines in red. `j` `k` scroll, `G` follows the end,
`Esc` closes.

**The efficiency view** (`e`) groups finished jobs by name, the way an array
or a pipeline rule produces many alike, and ends each row with a verdict:
`OOM x2: raise mem`, `TIMEOUT x1: raise runtime`, `near time limit`,
`mem over-requested: peak 500M of 8.0G`, or `ok`.

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
| `--bell` | | ring the terminal when a job of yours ends |
| `--on-event CMD` | | run a shell command when a job ends, with `HISSTOP_EVENT`, `HISSTOP_JOBID`, `HISSTOP_RULE` (the job name) and `HISSTOP_MESSAGE` in its environment |
| `--once` | | print a text snapshot and exit (also when stdout is not a terminal, e.g. `queuetop \| mail`) |
| `--json` | | print everything as JSON and exit: summary, queue with live usage, finished jobs, efficiency |

## How it works

* **Sources.** `squeue -u you -o ...` for the queue, `sstat -a -j ...` for the
  live usage of your running jobs (it only answers for your own jobs and only
  once a step is running), `sacct -u you -S since` for the history with steps
  folded into their job, and `scontrol show job` on demand when you open a
  popup, cached for 30 s.
* **Sentinels.** Fields Slurm has not sampled yet come back as 2^64 - 2 and
  print as absurd times (`213503982334-14:25:51`) or a `cpu%` in the
  trillions. queuetop treats those as unknown and shows `-`, and it also
  discards any CPU reading beyond what the allocation could have consumed.
* **Memory units.** `ReqMem` with the old per-CPU (`2000Mc`) and per-node
  (`8000Mn`) suffixes is converted to the job total; `MaxRSS` is the largest
  over the job's steps.
* **Array jobs.** Pending arrays show as `123_[1-3]`; running tasks as
  `123_4`, each with its own usage.
* **Cluster name.** From `SLURM_CLUSTER_NAME` when set, else the host name.

## Development

```bash
cargo test                        # parsers, the monitor against fake Slurm tools, screen renders, --once/--json
cargo clippy --all-targets
scripts/build-linux.sh [x86_64|aarch64] [scp-destination]
cargo run -q --example dump_screen -- main | python3 docs/render_screen.py docs/screenshot.png
```

queuetop depends on the hisstop crate from GitHub for the accounting units,
the verdicts, the tables, the popup machinery and the key handling.
`tests/fake/` holds stand-ins for `squeue`, `sstat`, `sacct`, `scontrol` and
`scancel` with canned output, including Slurm's not-yet-sampled sentinel.
Layout: `src/slurm.rs` (commands and parsers), `src/monitor.rs` (pollers),
`src/model.rs` (derived numbers), `src/text.rs` (tables, `--once`),
`src/ui.rs`, `src/app.rs`, `src/json.rs`. The popup screenshot is a real
capture from Dartmouth's Polaris cluster; the queue screenshot is rendered by
the real UI code from the fake tools (`examples/dump_screen.rs`).

## AI disclosure

Most of this repository was written by an AI. The Rust code, its tests, the
fake Slurm tools, the build script, the screenshot renderer and this README
were produced by Claude (Anthropic's Claude Fable 5.1, through Claude Code) in
sessions directed by Mike Martinez, who set the goals, chose the features,
reviewed the results and ran the tool on Dartmouth's Polaris and Discovery
clusters. That first real run is what exposed the sentinel bug described
above. Everything described here is covered by automated tests against the
fake tools, but Slurm's output varies between versions and sites: treat
`sstat` and `sacct` figures as advisory and report what looks wrong.
