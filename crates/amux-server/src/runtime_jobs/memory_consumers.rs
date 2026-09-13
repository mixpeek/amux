//! Pressure diagnostics must include compressed memory on macOS. RSS alone
//! hid a 24 GiB menu-bar process behind 70 MiB resident during AMUX-4417.
//! This snapshot is diagnostic only; it never selects processes to terminate.

use std::process::Command;
#[cfg(target_os = "macos")]
use std::{process::Stdio, time::Duration};
#[cfg(target_os = "macos")]
use wait_timeout::ChildExt;

#[derive(Debug, serde::Serialize)]
pub(super) struct Snapshot {
    pub measured: bool,
    pub n_considered: usize,
    pub metric: &'static str,
    pub why_unmeasured: Option<String>,
    pub consumers: Vec<Consumer>,
}

#[derive(Debug, serde::Serialize)]
pub(super) struct Consumer {
    pid: u32,
    command: String,
    bytes: u64,
    compressed_bytes: Option<u64>,
}

#[cfg(any(target_os = "macos", test))]
fn size_bytes(raw: &str) -> Option<u64> {
    let raw = raw.trim_end_matches(['+', '-']);
    let unit = raw.chars().last()?;
    let power = match unit {
        'B' => 0,
        'K' => 1,
        'M' => 2,
        'G' => 3,
        'T' => 4,
        _ => return None,
    };
    let number = raw[..raw.len() - 1].parse::<f64>().ok()?;
    let bytes = number * 1024_f64.powi(power);
    (bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

#[cfg(any(target_os = "macos", test))]
fn parse_top(raw: &str) -> Result<Vec<Consumer>, String> {
    let mut rows = Vec::new();
    let mut header = false;
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        if !header {
            header = fields.collect::<Vec<_>>() == ["PID", "MEM", "CMPRS", "COMMAND"];
            continue;
        }
        let Some(pid) = fields.next() else { continue };
        let row = (|| {
            let pid = pid.trim_end_matches(['*', '+', '-']).parse::<u32>().ok()?;
            let bytes = size_bytes(fields.next()?)?;
            let compressed_bytes = Some(size_bytes(fields.next()?)?);
            let command = fields.collect::<Vec<_>>().join(" ");
            (!command.is_empty()).then_some(Consumer {
                pid,
                command,
                bytes,
                compressed_bytes,
            })
        })()
        .ok_or_else(|| {
            format!(
                "top returned a malformed process row: {}",
                line.chars().take(160).collect::<String>()
            )
        })?;
        rows.push(row);
    }
    if !header || rows.is_empty() {
        return Err("top returned no measurable process rows".into());
    }
    Ok(rows)
}

#[cfg(target_os = "macos")]
fn bounded_output(cmd: &mut Command) -> Result<String, String> {
    // Only the top five rows are requested, keeping output below pipe capacity.
    // Absolute executable paths and C locale work under launchd as well.
    let mut child = cmd
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    match child.wait_timeout(Duration::from_secs(5)) {
        Ok(Some(status)) if status.success() => {}
        Ok(Some(status)) => return Err(format!("memory probe exited {status}")),
        result => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(match result {
                Err(error) => format!("memory probe wait failed: {error}"),
                _ => "memory probe timed out after 5s".into(),
            });
        }
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

pub(super) fn snapshot() -> Snapshot {
    #[cfg(target_os = "macos")]
    let (metric, result) = (
        "macos_top_mem_includes_compressed",
        bounded_output(Command::new("/usr/bin/top").args([
            "-l",
            "1",
            "-o",
            "mem",
            "-n",
            "5",
            "-stats",
            "pid,mem,cmprs,command",
        ]))
        .and_then(|raw| parse_top(&raw)),
    );
    #[cfg(not(target_os = "macos"))]
    let (metric, result) = ("rss_only", rss_snapshot());
    match result {
        Ok(consumers) => Snapshot {
            measured: true,
            n_considered: consumers.len(),
            metric,
            why_unmeasured: None,
            consumers,
        },
        Err(error) => Snapshot {
            measured: false,
            n_considered: 0,
            metric,
            why_unmeasured: Some(error),
            consumers: Vec::new(),
        },
    }
}

#[cfg(not(target_os = "macos"))]
fn rss_snapshot() -> Result<Vec<Consumer>, String> {
    let output = Command::new("ps")
        .args(["-eo", "pid=,rss=,comm="])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("ps exited {}", output.status));
    }
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let row = (|| {
            Some(Consumer {
                pid: fields.next()?.parse().ok()?,
                bytes: fields.next()?.parse::<u64>().ok()?.checked_mul(1024)?,
                command: fields.collect::<Vec<_>>().join(" "),
                compressed_bytes: None,
            })
        })()
        .ok_or_else(|| "ps returned a malformed process row".to_string())?;
        rows.push(row);
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.bytes));
    rows.truncate(5);
    if rows.is_empty() {
        return Err("ps returned no process rows".into());
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_hogs_and_spaced_names_survive_the_snapshot() {
        let rows = parse_top("Processes: 1206 total\nPhysMem: 95G used\n\nPID MEM CMPRS COMMAND\n567* 54G 49G fseventsd\n1715+ 24G 24G Python\n76850- 12G 12G Activity Monitor\n").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].bytes, 54 * 1024_u64.pow(3));
        assert_eq!(rows[1].compressed_bytes, Some(24 * 1024_u64.pow(3)));
        assert_eq!(rows[2].command, "Activity Monitor");
        assert_eq!(rows[2].pid, 76850);
    }

    #[test]
    fn missing_or_partial_measurements_cannot_look_healthy() {
        for text in [
            "",
            "permission denied",
            "PID MEM CMPRS COMMAND\n",
            "PID MEM CMPRS COMMAND\n1 24G ? Python",
            "PID MEM CMPRS COMMAND\n1 24G 23G Python\n2 broken",
        ] {
            assert!(parse_top(text).is_err(), "{text}");
        }
        for size in ["", "-1G", "NaNG", "infG", "99P", "1e50T", "10"] {
            assert_eq!(size_bytes(size), None, "{size}");
        }
        assert_eq!(size_bytes("1.5G+"), Some(1_610_612_736));
        assert_eq!(size_bytes("12M-"), Some(12 * 1024 * 1024));
        assert_eq!(size_bytes("0B"), Some(0));
    }

    #[test]
    fn native_memory_snapshot_is_measured_and_names_its_metric() {
        let result = snapshot();
        assert!(result.measured, "{result:?}");
        assert!(result.n_considered > 0);
        assert_eq!(result.n_considered, result.consumers.len());
        assert!(result.consumers.iter().any(|r| r.bytes > 0));
        #[cfg(target_os = "macos")]
        {
            assert_eq!(result.metric, "macos_top_mem_includes_compressed");
            assert!(result
                .consumers
                .iter()
                .all(|r| r.compressed_bytes.is_some()));
        }
    }
}
