//! Jobs of yours ended: `--bell` rings the terminal, `--on-event CMD` runs
//! a shell command with the details in its environment (a webhook,
//! `notify-send`, mail ...), so queuetop needs no HTTP client of its own.
//! Everything that ended in one poll goes out as one call.

use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub jobid: String,
    pub name: String,
    /// `COMPLETED`, `OOM kill`, `TIMEOUT`, `exit 1` ...
    pub state: String,
    pub bad: bool,
}

impl Event {
    /// One line for the notice area.
    pub fn describe(&self) -> String {
        if self.bad {
            format!("job {} ({}) ended: {}", self.jobid, self.name, self.state)
        } else {
            format!("job {} ({}) completed", self.jobid, self.name)
        }
    }
}

/// One line for several events: `3 jobs ended, 1 badly: 12 (a) OOM kill, 13 (b) completed ...`
pub fn describe_all(evs: &[Event]) -> String {
    match evs {
        [] => String::new(),
        [e] => e.describe(),
        _ => {
            let bad = evs.iter().filter(|e| e.bad).count();
            let shown: Vec<String> = evs.iter().take(3).map(|e| format!("{} ({}) {}", e.jobid, e.name, if e.bad { e.state.as_str() } else { "completed" })).collect();
            format!("{} jobs ended, {} badly: {}{}", evs.len(), bad, shown.join(", "), if evs.len() > 3 { " ..." } else { "" })
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Hooks {
    pub bell: bool,
    pub command: Option<String>,
}

impl Hooks {
    pub fn active(&self) -> bool {
        self.bell || self.command.is_some()
    }

    /// Ring once and/or run the command once for everything that ended in
    /// this poll. The command runs detached through `sh -c` with
    /// QUEUETOP_EVENT (`job_failed` if any ended badly, else `job_completed`),
    /// QUEUETOP_COUNT, QUEUETOP_JOBIDS (space separated), QUEUETOP_MESSAGE,
    /// and for the first job QUEUETOP_JOBID, QUEUETOP_NAME, QUEUETOP_STATE.
    pub fn fire(&self, evs: &[Event]) -> Result<(), String> {
        let Some(first) = evs.first() else { return Ok(()) };
        if self.bell {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07");
            let _ = out.flush();
        }
        if let Some(cmd) = &self.command {
            let ids: Vec<&str> = evs.iter().map(|e| e.jobid.as_str()).collect();
            Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .env("QUEUETOP_EVENT", if evs.iter().any(|e| e.bad) { "job_failed" } else { "job_completed" })
                .env("QUEUETOP_COUNT", evs.len().to_string())
                .env("QUEUETOP_JOBIDS", ids.join(" "))
                .env("QUEUETOP_JOBID", &first.jobid)
                .env("QUEUETOP_NAME", &first.name)
                .env("QUEUETOP_STATE", &first.state)
                .env("QUEUETOP_MESSAGE", describe_all(evs))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("--on-event: {}", e))?;
        }
        Ok(())
    }
}
