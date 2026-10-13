//! Host details for the web UI. Everything is best-effort: a field that
//! can't be read just comes back empty or zero and the page shows a dash.
//!
//! The figures are read fresh on every snapshot, but the sysinfo handles
//! they are read *through* are not rebuilt for it. Each snapshot sits on the
//! web UI's single-threaded runtime — every open event stream takes one
//! every two seconds — so time spent here is time the runtime can't answer
//! a request in. Building sysinfo's view of a thing costs far more than
//! re-reading it (enumerating every mount to measure one filesystem,
//! walking `/sys/class/hwmon` to read one sensor), so each of those views is
//! kept process wide and only the one figure the page shows is re-read.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use sysinfo::{
    Component, Components, CpuRefreshKind, DiskRefreshKind, Disks, MemoryRefreshKind, RefreshKind,
    System,
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
    let (mem_total, mem_available) = memory();
    let (disk_total, disk_available) = disk_info(workspace);
    Host {
        hostname: System::host_name().unwrap_or_default(),
        ip: local_ip(),
        cpu_usage: cpu_usage(),
        mem_total,
        mem_available,
        temp_c: cpu_temp(),
        disk_total,
        disk_available,
        uptime_secs: System::uptime(),
    }
}

/// How long any of the sysinfo views below stands before it is built again,
/// so a mount or a sensor that appears or disappears is still noticed.
const LIST_TTL: Duration = Duration::from_secs(60);

/// The System the memory figures are read through, kept across calls:
/// constructing one is itself a reading, so a fresh one per snapshot pays
/// for sysinfo's setup on top of the `/proc/meminfo` parse it is actually
/// after.
static MEMORY: Mutex<Option<System>> = Mutex::new(None);

/// Total and available RAM in bytes.
fn memory() -> (u64, u64) {
    let ram = MemoryRefreshKind::nothing().with_ram();
    let mut cached = MEMORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let system = match &mut *cached {
        // A freshly built System carries its first reading already.
        Some(system) => {
            system.refresh_memory_specifics(ram);
            system
        }
        none => none.insert(System::new_with_specifics(
            RefreshKind::nothing().with_memory(ram),
        )),
    };
    (system.total_memory(), system.available_memory())
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
    // The first call has no previous reading to diff against, so the figure
    // it gets is busy time since boot — the host's average rather than this
    // moment's, but a figure, and the next snapshot two seconds later
    // replaces it with a windowed one. Measuring a window here instead,
    // which is what sleeping through one amounted to, cost the web UI's
    // runtime half a second of the first page load.
    let sample = cached.get_or_insert_with(|| {
        let sys = System::new_with_specifics(
            RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing().with_cpu_usage()),
        );
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

/// The host's mounts, and which of them holds the workspace. Process wide
/// like `CPU` and `SENSORS`, and for the same reason: listing the mounts
/// walks `/proc/mounts` and `statvfs`es every entry on it, and every open
/// event stream asks for the same one filesystem's figures on its own tick.
/// Only the chosen mount is ever re-read, so a host with thirty of them
/// costs no more than one with a single root.
static DISKS: Mutex<Option<Volumes>> = Mutex::new(None);

struct Volumes {
    disks: Disks,
    /// Where in `disks` the mount holding `for_path` sits, settled when the
    /// list was built; None when no mount claims it.
    chosen: Option<usize>,
    /// The path `chosen` was picked for, so a snapshot of some other
    /// directory can't be answered with this one's filesystem.
    for_path: PathBuf,
    /// When the list itself was built, as opposed to re-read.
    listed: Instant,
}

/// Size and free space of the filesystem `path` lives on, in bytes — the
/// mount closest to it, since every ancestor's filesystem also "holds" it.
/// The free figure is what an unprivileged writer may actually use, so it
/// leaves out the root reserve.
fn disk_info(path: &Path) -> (u64, u64) {
    let storage = DiskRefreshKind::nothing().with_storage();
    let mut cached = DISKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let volumes = match &mut *cached {
        Some(volumes) if volumes.listed.elapsed() < LIST_TTL && volumes.for_path == path => {
            // A freshly built list carries its first reading already; a
            // standing one needs the chosen mount measured again.
            if let Some(index) = volumes.chosen {
                volumes.disks.list_mut()[index].refresh_specifics(storage);
            }
            volumes
        }
        _ => {
            let disks = Disks::new_with_refreshed_list_specifics(storage);
            let chosen = choose_mount(&disks, path);
            cached.insert(Volumes {
                disks,
                chosen,
                for_path: path.to_path_buf(),
                listed: Instant::now(),
            })
        }
    };
    let Some(index) = volumes.chosen else {
        return (0, 0);
    };
    let disk = &volumes.disks.list()[index];
    (disk.total_space(), disk.available_space())
}

/// Which of the host's mounts holds `path`: the one closest to it, since
/// every ancestor's filesystem also "holds" it.
fn choose_mount(disks: &Disks, path: &Path) -> Option<usize> {
    disks
        .list()
        .iter()
        .enumerate()
        .filter(|(_, disk)| path.starts_with(disk.mount_point()))
        .max_by_key(|(_, disk)| disk.mount_point().as_os_str().len())
        .map(|(index, _)| index)
}

/// The host's sensors, and which of them the page is shown, kept across
/// calls. Process wide like `CPU`: every open event stream asks for a
/// temperature on its own tick, and both halves of the work are wasted on
/// repeat. Listing the sensors walks `/sys/class/hwmon` and reads every
/// one's name, label, and thresholds, and reading them all out costs a file
/// read each — some of them, an NVMe drive's among them, served by the
/// device rather than the kernel and slow in proportion.
static SENSORS: Mutex<Option<Sensors>> = Mutex::new(None);

struct Sensors {
    components: Components,
    /// Where in `components` the sensor that speaks for the CPU sits,
    /// settled when the list was built; None when the host exposes no
    /// readable sensor at all. Only this one is ever read again, so a host
    /// with a dozen drive sensors costs no more than one with none.
    chosen: Option<usize>,
    /// When the list itself was built, as opposed to re-read.
    listed: Instant,
}

/// The CPU's temperature in degrees Celsius; None when the host exposes no
/// sensor to read it from.
fn cpu_temp() -> Option<f32> {
    let mut cached = SENSORS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sensors = match &mut *cached {
        Some(sensors) if sensors.listed.elapsed() < LIST_TTL => {
            // A freshly built list carries its first reading already; a
            // standing one needs the chosen sensor read out again.
            if let Some(index) = sensors.chosen {
                sensors.components.list_mut()[index].refresh();
            }
            sensors
        }
        _ => {
            let components = Components::new_with_refreshed_list();
            let chosen = choose_sensor(components.list());
            cached.insert(Sensors {
                components,
                chosen,
                listed: Instant::now(),
            })
        }
    };
    let index = sensors.chosen?;
    sensors.components.list()[index].temperature()
}

/// Label fragments that mark a sensor as the CPU's, best first: a
/// package-wide reading (Intel's `Package id 0`, AMD's `Tctl`/`Tdie`) beats
/// a generic CPU or SoC zone, which beats one die or core of it.
/// `k10temp` and `zenpower` are AMD's drivers, whose labels never say "cpu".
const CPU_SENSOR_KEYS: &[&str] = &[
    "pkg", "package", "tctl", "tdie", "cpu", "soc", "k10temp", "zenpower", "core",
];

/// Which of the host's sensors speaks for the CPU: one labeled as such, else
/// any readable sensor at all, so a board that labels nothing still shows a
/// figure. Sensors with no temperature to give are skipped, since a chosen
/// one has to stay readable on every later tick.
///
/// The pick depends only on the labels, never on where a sensor sits in the
/// list: sysinfo gathers each device's sensors through a HashMap, so their
/// order differs from one listing to the next, and a first-match pick would
/// show a different sensor every time the list is rebuilt.
fn choose_sensor(components: &[Component]) -> Option<usize> {
    components
        .iter()
        .enumerate()
        .filter(|(_, component)| component.temperature().is_some())
        .min_by_key(|(_, component)| {
            let label = component.label().to_lowercase();
            let rank = CPU_SENSOR_KEYS
                .iter()
                .position(|key| label.contains(key))
                .unwrap_or(CPU_SENSOR_KEYS.len());
            (rank, component.label())
        })
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reading back one remembered sensor has to answer what scanning every
    /// sensor does, since the full scan is what the page was shown before.
    /// A host that exposes no sensor at all — a container with no
    /// `/sys/class/hwmon` — agrees trivially, on None.
    #[test]
    fn the_remembered_sensor_answers_like_a_full_scan() {
        let all = Components::new_with_refreshed_list();
        let scanned = choose_sensor(all.list()).map(|at| &all.list()[at]);

        // Twice, so the second reading goes through the remembered list
        // rather than building one.
        assert_eq!(scanned.is_some(), cpu_temp().is_some());
        assert_eq!(scanned.is_some(), cpu_temp().is_some());
        // Both readings are live, and a busy host's CPU can move several
        // degrees between them, so the figures themselves aren't compared;
        // what must not differ is which sensor was read.
        let cached = SENSORS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remembered = cached
            .as_ref()
            .and_then(|sensors| sensors.chosen.map(|at| &sensors.components.list()[at]));
        assert_eq!(
            scanned.map(Component::label),
            remembered.map(Component::label),
            "the remembered sensor is not the one a full scan picks"
        );
    }

    /// The same contract for the mount list: measuring the one remembered
    /// mount has to answer what enumerating every mount does, since the full
    /// enumeration is what the page was shown before. And the memo is keyed
    /// on the path it chose for, so a second directory on a different
    /// filesystem is not answered with the first one's figures.
    #[test]
    fn the_remembered_mount_answers_like_a_full_scan() {
        let all =
            Disks::new_with_refreshed_list_specifics(DiskRefreshKind::nothing().with_storage());
        let scan = |path: &Path| {
            choose_mount(&all, path)
                .map(|at| {
                    let disk = &all.list()[at];
                    (disk.total_space(), disk.available_space())
                })
                .unwrap_or((0, 0))
        };

        // Twice, so the second reading goes through the remembered list
        // rather than building one. Total size can't move; the free figure
        // is live, so it is only required to stay on the same filesystem,
        // which a different mount would not.
        let root = Path::new("/");
        let (total, free) = disk_info(root);
        assert_eq!(total, scan(root).0);
        assert_eq!(disk_info(root).0, total);
        assert!(
            free.abs_diff(scan(root).1) < total / 100 + 1,
            "free space moved from {} to {free}",
            scan(root).1
        );

        // A path under /proc may belong to no listed mount (sysinfo leaves
        // the pseudo-filesystems out), or fall through to / on a host where
        // /proc isn't listed as its own mount; whatever it resolves to, the
        // remembered answer has to be its own. Free space is live, so it
        // gets the same tolerance as above.
        let other = Path::new("/proc/self");
        let (other_total, other_free) = disk_info(other);
        let (scan_total, scan_free) = scan(other);
        assert_eq!(other_total, scan_total);
        assert!(
            other_free.abs_diff(scan_free) < scan_total / 100 + 1,
            "free space moved from {scan_free} to {other_free}"
        );
        assert_eq!(disk_info(root).0, total, "the first path still answers");
    }
}
