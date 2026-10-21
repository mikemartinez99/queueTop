//! Wall clock with a test override: `QUEUETOP_NOW=<epoch seconds>` freezes
//! `now()` so a screen renders the same every time (tests, screenshots).

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> f64 {
    if let Ok(v) = std::env::var("QUEUETOP_NOW") {
        if let Ok(f) = v.trim().parse::<f64>() {
            return f;
        }
    }
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}
