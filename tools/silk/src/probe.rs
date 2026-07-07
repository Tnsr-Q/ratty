//! Capability probing: what a cast requires, what a terminal supports.
//!
//! `silk probe cast.silk` reports the RGP capabilities a cast depends on;
//! `silk probe --terminal` sends the RGP support query to the controlling
//! terminal and parses the one-line reply; together they answer
//! `playable: yes/no` by set containment. This is the tool an agent runs
//! to learn whether it is composing for a v1 or v2 ratty.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::validate::{Report, validate_file};

/// The RGP support query sequence.
const SUPPORT_QUERY: &[u8] = b"\x1b_ratty;g;s\x1b\\";
/// The reply prefix (a support *reply* carries fields after the verb).
const REPLY_PREFIX: &[u8] = b"\x1b_ratty;g;s;";

/// What a cast requires, derived from validation.
pub struct CastRequirements {
    /// v2 capability keys the cast depends on (empty = pure v1).
    pub capabilities: Vec<&'static str>,
    /// Asset formats the cast registers.
    pub formats: Vec<String>,
    /// Validation error count (a broken cast still reports requirements).
    pub errors: usize,
}

impl CastRequirements {
    fn from_report(report: &Report) -> Self {
        Self {
            capabilities: report.requires_v2.iter().copied().collect(),
            formats: report.formats.iter().cloned().collect(),
            errors: report.errors.len(),
        }
    }

    /// One-line human/agent-readable summary.
    pub fn summary(&self) -> String {
        let version = if self.capabilities.is_empty() {
            "RGP v1".to_string()
        } else {
            format!("RGP v2 ({})", self.capabilities.join(", "))
        };
        if self.formats.is_empty() {
            format!("requires: {version}")
        } else {
            format!("requires: {version}; formats: {}", self.formats.join(", "))
        }
    }
}

/// Probes a cast file for its capability requirements.
pub fn probe_cast(path: &Path) -> Result<CastRequirements> {
    let report = validate_file(path)?;
    Ok(CastRequirements::from_report(&report))
}

/// A parsed terminal capability map (`v` → `2`, `stage` → `1`, …).
pub type TerminalCapabilities = BTreeMap<String, String>;

/// Sends the RGP support query to the controlling terminal and parses the
/// reply. Errors when there is no controlling terminal; returns `Ok(None)`
/// when the terminal does not reply (not ratty).
pub fn query_terminal() -> Result<Option<TerminalCapabilities>> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("no controlling terminal (run inside ratty: `ratty -e silk probe --terminal`)")?;

    let saved = stty(&["-g"]).context("failed to save terminal settings")?;
    let result = (|| {
        // Raw-ish mode with a read timeout: min 0 time 10 makes read()
        // return empty after one second of silence instead of blocking.
        stty(&["raw", "-echo", "min", "0", "time", "10"])?;
        tty.write_all(SUPPORT_QUERY)?;
        tty.flush()?;

        let mut reply = Vec::new();
        let mut buffer = [0u8; 256];
        loop {
            let read = tty.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            reply.extend_from_slice(&buffer[..read]);
            if reply.len() > 4096
                || reply.windows(2).any(|window| window == b"\x1b\\")
                || reply.contains(&0x9c)
            {
                break;
            }
        }
        Ok::<_, anyhow::Error>(reply)
    })();
    // Always restore the terminal, even when the probe failed.
    let _ = stty(&[saved.trim()]);

    Ok(parse_reply(&result?))
}

/// Extracts the capability map from raw reply bytes.
pub fn parse_reply(reply: &[u8]) -> Option<TerminalCapabilities> {
    let start = reply
        .windows(REPLY_PREFIX.len())
        .position(|window| window == REPLY_PREFIX)?;
    let fields = &reply[start + REPLY_PREFIX.len()..];
    let end = fields
        .iter()
        .position(|&byte| byte == 0x1b || byte == 0x9c)
        .unwrap_or(fields.len());
    let content = std::str::from_utf8(&fields[..end]).ok()?;

    let mut capabilities = TerminalCapabilities::new();
    for part in content.split(';').filter(|part| !part.is_empty()) {
        let (key, value) = part.split_once('=')?;
        capabilities.insert(key.to_string(), value.to_string());
    }
    Some(capabilities)
}

/// Decides whether a cast's requirements are satisfied by a terminal's
/// capabilities. Returns the list of unmet requirements (empty = playable).
pub fn unmet_requirements(
    requirements: &CastRequirements,
    capabilities: &TerminalCapabilities,
) -> Vec<String> {
    let mut unmet = Vec::new();
    for capability in &requirements.capabilities {
        if capabilities.get(*capability).map(String::as_str) != Some("1") {
            unmet.push(format!("terminal does not advertise {capability}=1"));
        }
    }
    if !requirements.formats.is_empty() {
        let supported: Vec<&str> = capabilities
            .get("fmt")
            .map(|formats| formats.split('|').collect())
            .unwrap_or_default();
        for format in &requirements.formats {
            if !supported.contains(&format.as_str()) {
                unmet.push(format!("terminal does not support fmt={format}"));
            }
        }
    }
    unmet
}

/// Formats a capability map as the one-line summary `probe` prints.
pub fn capabilities_summary(capabilities: &TerminalCapabilities) -> String {
    let fields: Vec<String> = capabilities
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    format!("terminal supports: {}", fields.join("; "))
}

fn stty(args: &[&str]) -> Result<String> {
    let tty_in = std::fs::File::open("/dev/tty")?;
    let output = Command::new("stty")
        .args(args)
        .stdin(Stdio::from(tty_in))
        .output()
        .context("failed to run stty")?;
    if !output.status.success() {
        bail!(
            "stty {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).map_err(|_| anyhow!("stty output was not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2_reply() -> TerminalCapabilities {
        parse_reply(crate::rgp::support_reply().as_slice()).expect("reply parses")
    }

    #[test]
    fn parses_the_real_support_reply() {
        let capabilities = v2_reply();
        assert_eq!(capabilities.get("v").map(String::as_str), Some("2"));
        assert_eq!(capabilities.get("stage").map(String::as_str), Some("1"));
        assert_eq!(capabilities.get("tween").map(String::as_str), Some("1"));
        assert_eq!(capabilities.get("objanim").map(String::as_str), Some("1"));
        assert_eq!(
            capabilities.get("fmt").map(String::as_str),
            Some("obj|glb|stl")
        );
    }

    #[test]
    fn reply_parses_with_leading_noise_and_c1_terminator() {
        let mut reply = b"noise\x1b[1;1R".to_vec();
        reply.extend_from_slice(b"\x1b_ratty;g;s;v=2;stage=1");
        reply.push(0x9c);
        let capabilities = parse_reply(&reply).expect("parses");
        assert_eq!(capabilities.get("v").map(String::as_str), Some("2"));
    }

    #[test]
    fn no_reply_is_none() {
        assert!(parse_reply(b"").is_none());
        assert!(parse_reply(b"\x1b_ratty;g;s\x1b\\").is_none()); // bare query, not a reply
    }

    #[test]
    fn v2_cast_is_playable_on_v2_terminal_but_not_v1() {
        let requirements = CastRequirements {
            capabilities: vec!["stage", "tween"],
            formats: vec!["obj".to_string()],
            errors: 0,
        };
        assert!(unmet_requirements(&requirements, &v2_reply()).is_empty());

        let v1_reply = parse_reply(
            b"\x1b_ratty;g;s;v=1;fmt=obj|glb|stl;path=1;payload=1;chunk=1;anim=1;depth=1;\
              color=1;brightness=1;transform=1;update=1;normalize=1\x1b\\",
        )
        .expect("v1 reply parses");
        let unmet = unmet_requirements(&requirements, &v1_reply);
        assert_eq!(unmet.len(), 2);
        assert!(unmet[0].contains("stage"));
    }

    #[test]
    fn summary_formats_v1_and_v2() {
        let v1 = CastRequirements {
            capabilities: vec![],
            formats: vec!["obj".to_string()],
            errors: 0,
        };
        assert_eq!(v1.summary(), "requires: RGP v1; formats: obj");
        let v2 = CastRequirements {
            capabilities: vec!["objanim", "stage"],
            formats: vec![],
            errors: 0,
        };
        assert_eq!(v2.summary(), "requires: RGP v2 (objanim, stage)");
    }
}
