//! `~/.config/queuetop.toml`: defaults for the command line, one `key = value`
//! per line using the long option names (`since = "now-7days"`, `bell = true`,
//! `sstat-interval = 30`). The command line overrides the file.

use std::path::PathBuf;

/// `$QUEUETOP_CONFIG`, else `$XDG_CONFIG_HOME/queuetop.toml`, else `~/.config/queuetop.toml`.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("QUEUETOP_CONFIG") {
        return if p.is_empty() { None } else { Some(PathBuf::from(p)) };
    }
    let dir = std::env::var("XDG_CONFIG_HOME").ok().filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))?;
    Some(dir.join("queuetop.toml"))
}

/// `$XDG_CACHE_HOME/queuetop/cleared`, else `~/.cache/queuetop/cleared`: the finished jobs cleared with `C`.
pub fn cleared_path() -> Option<PathBuf> {
    let dir = std::env::var("XDG_CACHE_HOME").ok().filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".cache")))?;
    Some(dir.join("queuetop").join("cleared"))
}

/// The file's settings as command-line arguments, to go in front of the real ones.
pub fn config_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let key = k.trim().replace('_', "-");
        let v = v.trim().trim_matches('"').trim_matches('\'').trim();
        if key.is_empty() {
            continue;
        }
        match v {
            "true" => out.push(format!("--{}", key)),
            "false" | "" => {}
            _ => out.push(format!("--{}={}", key, v)),
        }
    }
    out
}
