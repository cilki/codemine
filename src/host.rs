//! Host details for the web UI, read fresh through sysinfo each time.
//! Everything is best-effort: a field that can't be read just comes back
//! empty or zero and the page shows a dash.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use sysinfo::{
    Components, CpuRefreshKind, DiskRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System,
};

#[derive(Serialize)]
pub struct Host {
    pub hostname: String,
    /// The address the default route leaves from; empty when unroutable.
    pub ip: String,
    /// Busy time across all cores as a percentage, 0-100.
    pub cpu_usage: f32,
    /// Bytes.
    pub mem_total: u64,
    pub mem_available: u64,
    /// Degrees Celsius; None when no sensor is exposed.
    pub temp_c: Option<f32>,
    /// Bytes on the filesystem holding the workspace, where the clones and
    /// turn logs pile up. Both zero when it can't be measured.
    pub disk_total: u64,
    pub disk_available: u64,
    pub uptime_secs: u64,
}

/// `workspace` picks the filesystem the disk figures describe; everything
/// else is the host as a whole.
pub fn snapshot(workspace: &Path) -> Host {
    let memory = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
    );
    let (disk_total, disk_available) = disk_info(workspace);
    Host {
        hostname: System::host_name().unwrap_or_default(),
        ip: local_ip(),
        cpu_usage: cpu_usage(),
        mem_total: memory.total_memory(),
        mem_available: memory.available_memory(),
        temp_c: cpu_temp(),
        disk_total,
        disk_available,
        uptime_secs: System::uptime(),
    }
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

/// The System the usage is diffed through and the figure it last produced.
/// Kept process wide so every caller — each open event stream, plus
/// `/api/host` — shares one measurement window instead of racing each other
/// for ever shorter, ever noisier deltas.
static CPU: Mutex<Option<CpuSample>> = Mutex::new(None);

struct CpuSample {
    /// Carries the previous reading sysinfo diffs the next refresh against.
    sys: System,
    at: Instant,
    usage: f32,
}

/// The shortest span a usage figure is measured over; below this it is
/// mostly quantization noise.
const CPU_WINDOW: Duration = Duration::from_millis(500);

/// Busy time as a percentage of all CPU time since the previous reading.
fn cpu_usage() -> f32 {
    let mut cached = CPU.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // The first call has nothing to diff against, so it measures a window of
    // its own rather than leave the page blank.
    let sample = cached.get_or_insert_with(|| {
        let mut sys = System::new_with_specifics(
            RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing().with_cpu_usage()),
        );
        std::thread::sleep(CPU_WINDOW);
        sys.refresh_cpu_usage();
        CpuSample {
            usage: sys.global_cpu_usage(),
            at: Instant::now(),
            sys,
        }
    });
    // Asked again within the window, the answer hasn't had time to change,
    // and re-diffing would only add noise.
    if sample.at.elapsed() >= CPU_WINDOW {
        sample.sys.refresh_cpu_usage();
        sample.at = Instant::now();
        sample.usage = sample.sys.global_cpu_usage();
    }
    sample.usage
}

/// Size and free space of the filesystem `path` lives on, in bytes — the
/// mount closest to it, since every ancestor's filesystem also "holds" it.
/// The free figure is what an unprivileged writer may actually use, so it
/// leaves out the root reserve.
fn disk_info(path: &Path) -> (u64, u64) {
    Disks::new_with_refreshed_list_specifics(DiskRefreshKind::nothing().with_storage())
        .list()
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| (disk.total_space(), disk.available_space()))
        .unwrap_or((0, 0))
}

/// The CPU's sensor if one is labeled as such, else any sensor at all.
fn cpu_temp() -> Option<f32> {
    let mut fallback = None;
    for component in Components::new_with_refreshed_list().list() {
        let Some(temp) = component.temperature() else {
            continue;
        };
        let label = component.label().to_lowercase();
        if ["cpu", "pkg", "core", "soc"]
            .iter()
            .any(|k| label.contains(k))
        {
            return Some(temp);
        }
        fallback.get_or_insert(temp);
    }
    fallback
}
