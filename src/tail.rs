//! A cheap tail of a job's output file for the popup, re-read only when the
//! file changes.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::clock;
use crate::text::fmt_age;

static RE_PROBLEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(error|exception|traceback|killed|oom|out of memory|segfault|segmentation fault|cancelled|time ?limit|no such file|permission denied|command not found)\b").unwrap()
});

/// Does a log line look like trouble? Used to colour the tail.
pub fn is_problem_line(line: &str) -> bool {
    RE_PROBLEM.is_match(line)
}

pub fn fmt_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Show a path relative to the workdir when it is under it.
pub fn display_path(workdir: &Path, p: &Path) -> String {
    match p.strip_prefix(workdir) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => p.display().to_string(),
    }
}

#[derive(Clone, Debug, Default)]
pub struct Tail {
    pub lines: Vec<String>,
    pub size: u64,
    /// last modification, epoch seconds
    pub mtime: Option<f64>,
}

impl Tail {
    pub fn describe(&self) -> String {
        let mut s = fmt_size(self.size);
        if let Some(m) = self.mtime {
            s.push_str(&format!(", modified {} ago", fmt_age(Some(clock::now() - m))));
        }
        s
    }
}

fn mtime_of(md: &fs::Metadata) -> Option<f64> {
    md.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64())
}

/// The last `max_bytes` of `path`, split into lines (a partial first line is dropped).
pub fn tail(path: &Path, max_bytes: u64) -> io::Result<Tail> {
    let mut fh = fs::File::open(path)?;
    let md = fh.metadata()?;
    let size = md.len();
    let start = size.saturating_sub(max_bytes);
    fh.seek(SeekFrom::Start(start))?;
    let mut data = Vec::new();
    fh.read_to_end(&mut data)?;
    let text = String::from_utf8_lossy(&data);
    let mut lines: Vec<String> = text.lines().map(|l| l.trim_end_matches('\r').to_string()).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(Tail { lines, size, mtime: mtime_of(&md) })
}

/// The tail of whichever file was asked for last, re-read only when its size
/// or modification time changed (the files live on cluster storage).
#[derive(Debug, Default)]
pub struct TailCache {
    path: PathBuf,
    tail: Option<Tail>,
}

impl TailCache {
    pub fn get(&mut self, path: &Path, max_bytes: u64) -> io::Result<&Tail> {
        let md = fs::metadata(path)?;
        let fresh = self.path == path && self.tail.as_ref().is_some_and(|t| t.size == md.len() && t.mtime == mtime_of(&md));
        if !fresh {
            self.tail = Some(tail(path, max_bytes)?);
            self.path = path.to_path_buf();
        }
        Ok(self.tail.as_ref().unwrap())
    }
}
