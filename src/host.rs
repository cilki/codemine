//! Host details for the web UI, read fresh from /proc and /sys each time.
//! Everything is best-effort: a field that can't be read just comes back
//! empty or zero and the page shows a dash.

use serde::Serialize;

#[derive(Serialize, Default)]
pub struct Host {
    pub hostname: String,
    /// The address the default route leaves from; empty when unroutable.
    pub ip: String,
    pub cpu_model: String,
    pub cpus: usize,
    /// Bytes.
    pub mem_total: u64,
    pub mem_available: u64,
    /// Degrees Celsius; None when no sensor is exposed.
    pub temp_c: Option<f32>,
    pub uptime_secs: u64,
    /// Cumulative since boot, all interfaces except loopback.
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

pub fn snapshot() -> Host {
    let (cpu_model, cpus) = cpu_info();
    let (mem_total, mem_available) = mem_info();
    let (rx_bytes, tx_bytes) = net_totals();
    Host {
        hostname: read_trimmed("/proc/sys/kernel/hostname"),
        ip: local_ip(),
        cpu_model,
        cpus,
        mem_total,
        mem_available,
        temp_c: cpu_temp(),
        uptime_secs: read_trimmed("/proc/uptime")
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0) as u64,
        rx_bytes,
        tx_bytes,
    }
}

fn read_trimmed(path: &str) -> String {
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

fn cpu_info() -> (String, usize) {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let model = info
        .lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, model)| model.trim().to_owned())
        .unwrap_or_default();
    let cpus = info
        .lines()
        .filter(|line| line.starts_with("processor"))
        .count();
    let cpus = match cpus {
        0 => std::thread::available_parallelism().map_or(0, |n| n.get()),
        n => n,
    };
    (model, cpus)
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
        let kind = read_trimmed(&path.join("type").display().to_string()).to_lowercase();
        if ["cpu", "pkg", "core", "soc"].iter().any(|k| kind.contains(k)) {
            return Some(temp);
        }
        fallback.get_or_insert(temp);
    }
    fallback
}

/// Cumulative receive and transmit bytes across every interface but lo.
fn net_totals() -> (u64, u64) {
    let dev = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let (mut rx, mut tx) = (0, 0);
    for line in dev.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() == "lo" {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        rx += fields.first().and_then(|f| f.parse().ok()).unwrap_or(0u64);
        tx += fields.get(8).and_then(|f| f.parse().ok()).unwrap_or(0u64);
    }
    (rx, tx)
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
    }
}
