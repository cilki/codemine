//! Host details for the web UI, read fresh from /proc and /sys each time.
//! Everything is best-effort: a field that can't be read just comes back
//! empty or zero and the page shows a dash.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Serialize, Default)]
pub struct Host {
    pub hostname: String,
    /// The address the default route leaves from; empty when unroutable.
    pub ip: String,
    pub cpus: usize,
    /// Busy time across all cores as a percentage, 0-100; None before the
    /// first measurement window closes.
    pub cpu_usage: Option<f32>,
    /// Bytes.
    pub mem_total: u64,
    pub mem_available: u64,
    /// Degrees Celsius; None when no sensor is exposed.
    pub temp_c: Option<f32>,
    pub uptime_secs: u64,
}

pub fn snapshot() -> Host {
    let (mem_total, mem_available) = mem_info();
    Host {
        hostname: read_trimmed("/proc/sys/kernel/hostname"),
        ip: local_ip(),
        cpus: cpu_count(),
        cpu_usage: cpu_usage(),
        mem_total,
        mem_available,
        temp_c: cpu_temp(),
        uptime_secs: read_trimmed("/proc/uptime")
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0) as u64,
    }
}

fn read_trimmed(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// The source address of a would-be packet to a public host; connecting a
/// UDP socket sends nothing but resolves the route.
fn local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("1.1.1.1:80")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_default()
}

fn cpu_count() -> usize {
    let cpus = std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with("processor"))
        .count();
    match cpus {
        0 => std::thread::available_parallelism().map_or(0, |n| n.get()),
        n => n,
    }
}

/// The last /proc/stat reading and the usage computed from it. Kept process
/// wide so every caller — each open event stream, plus `/api/host` — shares
/// one measurement window instead of racing each other for ever shorter,
/// ever noisier deltas.
static CPU: Mutex<Option<CpuSample>> = Mutex::new(None);

struct CpuSample {
    at: Instant,
    /// Jiffies since boot, all of them and the idle ones.
    total: u64,
    idle: u64,
    usage: f32,
}

/// The shortest span a usage figure is measured over; below this it is
/// mostly quantization noise.
const CPU_WINDOW: Duration = Duration::from_millis(500);

/// Busy time as a percentage of all CPU time since the previous reading.
fn cpu_usage() -> Option<f32> {
    let mut cached = CPU.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut now = cpu_times()?;
    let (prev_total, prev_idle) = match cached.take() {
        // Asked again within the window: the answer hasn't had time to
        // change, and re-diffing would only add noise.
        Some(prev) if prev.at.elapsed() < CPU_WINDOW => {
            let usage = prev.usage;
            *cached = Some(prev);
            return Some(usage);
        }
        Some(prev) => (prev.total, prev.idle),
        // The first call has nothing to diff against, so it measures a
        // window of its own rather than leave the page blank.
        None => {
            let first = now;
            std::thread::sleep(CPU_WINDOW);
            now = cpu_times()?;
            first
        }
    };
    let (total, idle) = now;
    let elapsed = total.saturating_sub(prev_total);
    let idled = idle.saturating_sub(prev_idle);
    let usage = match elapsed {
        0 => 0.0,
        _ => 100.0 * elapsed.saturating_sub(idled) as f32 / elapsed as f32,
    };
    *cached = Some(CpuSample {
        at: Instant::now(),
        total,
        idle,
        usage,
    });
    Some(usage)
}

/// Total and idle jiffies since boot, from /proc/stat's summary line.
fn cpu_times() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let fields: Vec<u64> = stat
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .filter_map(|field| field.parse().ok())
        .collect();
    // user nice system idle iowait irq softirq steal ...; idle and iowait
    // are both time the CPU had nothing to run.
    let idle = *fields.get(3)? + *fields.get(4)?;
    Some((fields.iter().sum(), idle))
}

/// MemTotal and MemAvailable in bytes.
fn mem_info() -> (u64, u64) {
    let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| {
        info.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

/// The CPU's thermal zone if one is labeled as such, else any zone at all.
fn cpu_temp() -> Option<f32> {
    let mut fallback = None;
    for entry in std::fs::read_dir("/sys/class/thermal").ok()?.flatten() {
        let path = entry.path();
        let Some(milli) = std::fs::read_to_string(path.join("temp"))
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
        else {
            continue;
        };
        let temp = milli / 1000.0;
        let kind = read_trimmed(path.join("type")).to_lowercase();
        if ["cpu", "pkg", "core", "soc"]
            .iter()
            .any(|k| kind.contains(k))
        {
            return Some(temp);
        }
        fallback.get_or_insert(temp);
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reads_the_linux_procfs() {
        let host = snapshot();
        assert!(!host.hostname.is_empty());
        assert!(host.cpus > 0);
        assert!(host.mem_total > 0);
        assert!(host.mem_available <= host.mem_total);
        assert!(host.uptime_secs > 0);
        let usage = host
            .cpu_usage
            .expect("the first call measures its own window");
        assert!((0.0..=100.0).contains(&usage), "{usage}");
    }

    #[test]
    fn cpu_times_only_move_forward() {
        let (total, idle) = cpu_times().expect("/proc/stat is readable");
        assert!(total > 0 && idle <= total, "{idle} of {total}");
        let (later, later_idle) = cpu_times().unwrap();
        assert!(later >= total && later_idle >= idle);
    }
}
