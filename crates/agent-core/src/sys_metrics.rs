//! Process CPU/memory sampling for the stress lab (Linux `/proc`).
//!
//! Reads `/proc/self/stat` and `/proc/self/status` directly — no extra
//! dependencies. On non-Linux platforms [`SysMonitor::sample`] returns `None`
//! (the stress lab still runs, it just omits CPU/RSS columns).

use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SysSample {
    /// Percent of one core (top convention: 100.0 = one fully busy core; can
    /// exceed 100 on many cores).
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    /// High-water mark from `VmHWM` — the true lifetime peak.
    pub peak_rss_bytes: u64,
    pub threads: u32,
}

/// Differential CPU sampler. Feed it at a steady interval (250ms–1s).
pub struct SysMonitor {
    /// `(utime, stime)` clock ticks from the previous sample.
    last_ticks: Option<(u64, u64)>,
    last_instant: Option<Instant>,
    cores: f32,
}

impl SysMonitor {
    pub fn new() -> Self {
        Self {
            last_ticks: None,
            last_instant: None,
            cores: std::thread::available_parallelism()
                .map(|n| n.get() as f32)
                .unwrap_or(1.0),
        }
    }

    pub fn cores(&self) -> f32 {
        self.cores
    }

    pub fn sample(&mut self) -> Option<SysSample> {
        let stat = read_proc_stat()?;
        let now = Instant::now();

        let cpu_percent = match (self.last_ticks, self.last_instant) {
            (Some((lu, ls)), Some(prev)) => {
                let dt = now.duration_since(prev).as_secs_f64();
                if dt > 0.0 {
                    // CLK_TCK is 100 on essentially every Linux userspace.
                    const CLK_TCK: f64 = 100.0;
                    let busy_ticks =
                        (stat.utime.saturating_sub(lu) + stat.stime.saturating_sub(ls)) as f64;
                    ((busy_ticks / CLK_TCK) / dt * 100.0) as f32
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        self.last_ticks = Some((stat.utime, stat.stime));
        self.last_instant = Some(now);

        let status = read_proc_status()?;
        Some(SysSample {
            cpu_percent,
            rss_bytes: status.vm_rss_kib * 1024,
            peak_rss_bytes: status.vm_hwm_kib * 1024,
            threads: status.threads,
        })
    }
}

impl Default for SysMonitor {
    fn default() -> Self {
        Self::new()
    }
}

struct ProcStat {
    utime: u64,
    stime: u64,
}

struct ProcStatus {
    vm_rss_kib: u64,
    vm_hwm_kib: u64,
    threads: u32,
}

fn read_proc_stat() -> Option<ProcStat> {
    let raw = std::fs::read_to_string("/proc/self/stat").ok()?;
    // comm (field 2) may contain spaces and parentheses; fields resume after
    // the LAST ')'. Post-paren index 0 is field 3, so utime (14) = 11, stime
    // (15) = 12.
    let after_paren = raw.rsplit_once(')')?.1.split_whitespace();
    let fields: Vec<&str> = after_paren.collect();
    if fields.len() < 13 {
        return None;
    }
    Some(ProcStat {
        utime: fields[11].parse().ok()?,
        stime: fields[12].parse().ok()?,
    })
}

fn read_proc_status() -> Option<ProcStatus> {
    let raw = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |key: &str| -> Option<u64> {
        raw.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim().eq_ignore_ascii_case(key).then(|| {
                value
                    .trim()
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
            })?
        })
    };
    let threads = field("Threads").unwrap_or(0) as u32;
    Some(ProcStatus {
        vm_rss_kib: field("VmRSS")?,
        vm_hwm_kib: field("VmHWM").unwrap_or(0),
        threads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_on_linux() {
        // Trivially skip on non-Linux; on Linux /proc/self must parse.
        if !std::path::Path::new("/proc/self/stat").exists() {
            return;
        }
        let mut monitor = SysMonitor::new();
        let first = monitor.sample().expect("first sample");
        assert!(first.rss_bytes > 0);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let second = monitor.sample().expect("second sample");
        // Sanity: CPU% within a plausible band for a short idle gap.
        assert!(second.cpu_percent >= 0.0 && second.cpu_percent < 10_000.0);
        assert!(second.peak_rss_bytes >= second.rss_bytes || second.peak_rss_bytes == 0);
    }
}
