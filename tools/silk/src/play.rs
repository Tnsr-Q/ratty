//! Timed cast replay to stdout. Run it inside ratty for the native preview:
//! `ratty -e silk play <cast>`.

use std::io::{self, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::cast::read_cast;

/// Plays a cast to stdout, pacing events by their timestamps.
pub fn play_file(path: &Path, speed: f64, looped: bool) -> Result<()> {
    let cast = read_cast(path)?;
    let speed = if speed > 0.0 { speed } else { 1.0 };
    let idle_cap = cast.header.idle_time_limit.filter(|limit| *limit > 0.0);

    let stdout = io::stdout();
    loop {
        let mut clock = 0.0f64;
        for event in &cast.events {
            let mut gap = (event.time - clock).max(0.0);
            if let Some(cap) = idle_cap {
                gap = gap.min(cap);
            }
            if gap > 0.0 {
                thread::sleep(Duration::from_secs_f64(gap / speed));
            }
            clock = event.time;
            if event.code == "o" {
                let mut handle = stdout.lock();
                handle.write_all(event.data.as_bytes())?;
                handle.flush()?;
            }
        }
        let header_loops = cast
            .header
            .x_ratty
            .as_ref()
            .is_some_and(|x| x.loop_ == Some(true));
        if !looped && !header_loops {
            break;
        }
        // Brief beat between loops so the restart reads as intentional.
        thread::sleep(Duration::from_secs_f64(0.75 / speed));
    }
    Ok(())
}
