//! Turns `/proc` and `/sys` into a [`Snapshot`].
//!
//! Rates (CPU busy percentage, network throughput) are deltas between two
//! readings, so the first call cannot produce them and returns `None` rather
//! than a snapshot full of zeroes that would render a busy node as idle.
//!
//! The filesystem roots are injectable so the whole sampler can be tested
//! against fixture directories.

use std::path::{Path, PathBuf};
use std::time::Instant;

use icecc_metrics::{Cpu, Interface, Net, Sensor, Snapshot, Thermal, SCHEMA_VERSION};

use crate::parse::{self, CpuTimes, IfCounters, RawSensor};

struct Previous {
    at: Instant,
    total: CpuTimes,
    per_core: Vec<CpuTimes>,
    interfaces: Vec<IfCounters>,
}

pub struct Sampler {
    proc_root: PathBuf,
    sys_root: PathBuf,
    hostname: String,
    addresses: Vec<String>,
    previous: Option<Previous>,
}

impl Sampler {
    pub fn new() -> Self {
        Self::with_roots("/proc", "/sys")
    }

    pub fn with_roots(proc_root: impl Into<PathBuf>, sys_root: impl Into<PathBuf>) -> Self {
        let proc_root = proc_root.into();
        let hostname = read_trimmed(&proc_root.join("sys/kernel/hostname"))
            .unwrap_or_else(|| "unknown".to_owned());
        Self {
            proc_root,
            sys_root: sys_root.into(),
            hostname,
            addresses: local_addresses(),
            previous: None,
        }
    }

    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Read everything once. `None` on the first call, when no rate can be
    /// computed yet.
    pub fn sample(&mut self) -> Option<Snapshot> {
        let now = Instant::now();

        let stat_text = read_to_string(&self.proc_root.join("stat")).unwrap_or_default();
        let (total, per_core) = parse::stat(&stat_text);
        let total = total.unwrap_or_default();

        let interfaces =
            parse::net_dev(&read_to_string(&self.proc_root.join("net/dev")).unwrap_or_default());

        let previous = self.previous.replace(Previous {
            at: now,
            total,
            per_core: per_core.clone(),
            interfaces: interfaces.clone(),
        });
        let previous = previous?;

        let elapsed_ms = now.duration_since(previous.at).as_millis() as u64;
        // Two samples inside the same millisecond give no window to average
        // rates over, and reporting 0 B/s would read as an idle network. The
        // new baseline is already stored, so the next call succeeds.
        if elapsed_ms == 0 {
            return None;
        }

        let cpu = Cpu {
            cores: per_core.len(),
            total_busy_pct: total.busy_pct_since(&previous.total).unwrap_or(0.0),
            per_core_busy_pct: per_core
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    // A core that appeared since the last sample (hotplug) has
                    // nothing to diff against.
                    previous
                        .per_core
                        .get(i)
                        .and_then(|p| c.busy_pct_since(p))
                        .unwrap_or(0.0)
                })
                .collect(),
            freq_mhz: self.read_frequencies(per_core.len()),
        };

        let net = self.rates(&interfaces, &previous.interfaces, elapsed_ms);

        let mem =
            parse::meminfo(&read_to_string(&self.proc_root.join("meminfo")).unwrap_or_default());
        let load =
            parse::loadavg(&read_to_string(&self.proc_root.join("loadavg")).unwrap_or_default())
                .unwrap_or(icecc_metrics::Load {
                    one: 0.0,
                    five: 0.0,
                    fifteen: 0.0,
                    runnable: 0,
                    total_procs: 0,
                });
        let uptime_secs = read_to_string(&self.proc_root.join("uptime"))
            .and_then(|t| parse::uptime_secs(&t))
            .unwrap_or(0);

        Some(Snapshot {
            schema: SCHEMA_VERSION,
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            hostname: self.hostname.clone(),
            addresses: self.addresses.clone(),
            uptime_secs,
            sampled_unix_ms: unix_millis(),
            sample_interval_ms: elapsed_ms,
            cpu,
            mem,
            load,
            thermal: self.read_thermal(),
            net,
        })
    }

    fn rates(&self, now: &[IfCounters], earlier: &[IfCounters], elapsed_ms: u64) -> Net {
        let mut interfaces = Vec::with_capacity(now.len());
        let mut rx_total = 0u64;
        let mut tx_total = 0u64;

        for iface in now {
            // Match by name, not by index: interfaces come and go.
            let Some(prev) = earlier.iter().find(|p| p.name == iface.name) else {
                continue;
            };
            let rx = parse::rate_per_sec(iface.rx_bytes, prev.rx_bytes, elapsed_ms);
            let tx = parse::rate_per_sec(iface.tx_bytes, prev.tx_bytes, elapsed_ms);
            if parse::is_aggregated(&iface.name) {
                rx_total += rx;
                tx_total += tx;
            }
            interfaces.push(Interface {
                name: iface.name.clone(),
                rx_bytes_per_sec: rx,
                tx_bytes_per_sec: tx,
            });
        }

        Net {
            rx_bytes_per_sec: rx_total,
            tx_bytes_per_sec: tx_total,
            interfaces,
        }
    }

    /// Current clock per core from `cpufreq`. Empty where the kernel does not
    /// expose it (many VMs and containers).
    fn read_frequencies(&self, cores: usize) -> Vec<u32> {
        let mut out = Vec::new();
        for i in 0..cores {
            let path = self.sys_root.join(format!(
                "devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq"
            ));
            match read_trimmed(&path).and_then(|t| t.parse::<u64>().ok()) {
                // The file is in kHz.
                Some(khz) => out.push((khz / 1000) as u32),
                None => return Vec::new(),
            }
        }
        out
    }

    fn read_thermal(&self) -> Thermal {
        let mut sensors = self.read_hwmon();
        sensors.extend(self.read_thermal_zones());

        // A reading of exactly 0 means "no sensor fitted" in several drivers
        // (thinkpad_acpi is the common one), and lm-sensors reports those as
        // N/A. Keeping them would put a 0 °C row in the detail view.
        sensors.retain(|s| s.celsius != 0.0);

        // The same physical sensor is often exposed through both hwmon and a
        // thermal zone, e.g. acpitz. Keep the first of each name.
        let mut seen = std::collections::HashSet::new();
        sensors.retain(|s| seen.insert(s.qualified()));

        let picked = parse::pick_cpu_sensor(&sensors);
        Thermal {
            cpu_celsius: picked.map(|s| s.celsius),
            cpu_source: picked.map(|s| s.qualified()),
            sensors: sensors
                .iter()
                .map(|s| Sensor {
                    label: s.qualified(),
                    celsius: s.celsius,
                })
                .collect(),
        }
    }

    /// `/sys/class/hwmon/hwmonN/{name,tempN_input,tempN_label}`.
    fn read_hwmon(&self) -> Vec<RawSensor> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.sys_root.join("class/hwmon")) else {
            return out;
        };
        let mut chips: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        chips.sort();

        for chip_dir in chips {
            let chip = read_trimmed(&chip_dir.join("name")).unwrap_or_else(|| "hwmon".to_owned());
            let Ok(files) = std::fs::read_dir(&chip_dir) else {
                continue;
            };
            let mut inputs: Vec<PathBuf> = files
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("temp") && n.ends_with("_input"))
                })
                .collect();
            // temp2_input must not sort before temp10_input by chance; numeric
            // order keeps "Core 0" before "Core 1" in the reported list.
            inputs.sort_by_key(|p| sensor_index(p));

            for input in inputs {
                let Some(celsius) = read_trimmed(&input).and_then(|t| parse::millidegrees(&t))
                else {
                    continue;
                };
                let stem = input
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.trim_end_matches("_input").to_owned())
                    .unwrap_or_default();
                let label = read_trimmed(&input.with_file_name(format!("{stem}_label")))
                    .unwrap_or_else(|| stem.clone());
                out.push(RawSensor {
                    chip: chip.clone(),
                    label,
                    celsius,
                });
            }
        }
        out
    }

    /// `/sys/class/thermal/thermal_zoneN/{type,temp}`.
    fn read_thermal_zones(&self) -> Vec<RawSensor> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.sys_root.join("class/thermal")) else {
            return out;
        };
        let mut zones: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("thermal_zone"))
            })
            .collect();
        zones.sort();

        for zone in zones {
            let Some(celsius) =
                read_trimmed(&zone.join("temp")).and_then(|t| parse::millidegrees(&t))
            else {
                continue;
            };
            let chip = read_trimmed(&zone.join("type")).unwrap_or_else(|| "thermal".to_owned());
            out.push(RawSensor {
                chip,
                label: "temp1".to_owned(),
                celsius,
            });
        }
        out
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Numeric part of `tempN_input`, for sorting.
fn sensor_index(path: &Path) -> u32 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("temp"))
        .and_then(|n| n.strip_suffix("_input"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

fn read_to_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Every non-loopback address, so the monitor can match this host however the
/// scheduler happens to know it.
fn local_addresses() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() {
                continue;
            }
            out.push(iface.ip().to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake `/proc` and `/sys` so the sampler can be exercised without
    /// depending on the machine running the tests.
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "icecream-watcher-agent-test-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let f = Self { dir };
            f.write("proc/sys/kernel/hostname", "build01\n");
            f.write("proc/meminfo", MEMINFO);
            f.write("proc/loadavg", "2.49 3.81 8.67 1/4540 541387\n");
            f.write("proc/uptime", "275290.06 2253871.58\n");
            f
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        /// Samples must be separated by a measurable window; see `sample`.
        fn tick() {
            std::thread::sleep(std::time::Duration::from_millis(3));
        }

        fn sampler(&self) -> Sampler {
            Sampler::with_roots(self.dir.join("proc"), self.dir.join("sys"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Verbatim from `/proc/meminfo` on the development machine.
    const MEMINFO: &str = "\
MemTotal:       65540720 kB
MemFree:        16456404 kB
MemAvailable:   33357408 kB
Buffers:          759784 kB
Cached:         18495264 kB
SwapTotal:       8388604 kB
SwapFree:         151148 kB
";

    const STAT_T0: &str =
        "cpu  1000 0 0 3000 0 0 0 0\ncpu0 500 0 0 1500 0 0 0 0\ncpu1 500 0 0 1500 0 0 0 0\n";
    // 400 more jiffies total on cpu, 100 idle -> 75 % busy.
    const STAT_T1: &str =
        "cpu  1300 0 0 3100 0 0 0 0\ncpu0 700 0 0 1550 0 0 0 0\ncpu1 600 0 0 1550 0 0 0 0\n";

    const NET_T0: &str = "Inter-|   Receive | Transmit\n face |bytes\n    lo: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n  eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n";
    const NET_T1: &str = "Inter-|   Receive | Transmit\n face |bytes\n    lo: 5000 0 0 0 0 0 0 0 5000 0 0 0 0 0 0 0\n  eth0: 2000 0 0 0 0 0 0 0 6000 0 0 0 0 0 0 0\n";

    #[test]
    fn the_first_sample_yields_nothing_because_there_is_no_delta() {
        let f = Fixture::new("first");
        f.write("proc/stat", STAT_T0);
        f.write("proc/net/dev", NET_T0);
        let mut s = f.sampler();
        assert!(
            s.sample().is_none(),
            "a first snapshot would show a busy node as 0% CPU"
        );
    }

    #[test]
    fn the_second_sample_carries_rates_from_the_delta() {
        let f = Fixture::new("second");
        f.write("proc/stat", STAT_T0);
        f.write("proc/net/dev", NET_T0);
        let mut s = f.sampler();
        assert!(s.sample().is_none());

        Fixture::tick();
        f.write("proc/stat", STAT_T1);
        f.write("proc/net/dev", NET_T1);
        let snap = s.sample().expect("second sample");

        assert_eq!(snap.hostname, "build01");
        assert_eq!(snap.cpu.cores, 2);
        assert_eq!(snap.cpu.total_busy_pct, 75.0);
        assert_eq!(snap.cpu.per_core_busy_pct.len(), 2);
        assert_eq!(snap.mem.total_kib, 65_540_720);
        assert_eq!(snap.load.one, 2.49);
        assert_eq!(snap.uptime_secs, 275_290);
        assert!(snap.schema_supported());
    }

    #[test]
    fn loopback_traffic_is_excluded_from_the_aggregate() {
        let f = Fixture::new("net");
        f.write("proc/stat", STAT_T0);
        f.write("proc/net/dev", NET_T0);
        let mut s = f.sampler();
        s.sample();
        Fixture::tick();
        f.write("proc/stat", STAT_T1);
        f.write("proc/net/dev", NET_T1);
        let snap = s.sample().unwrap();

        // lo moved 5000 bytes and eth0 1000; only eth0 counts.
        assert!(snap.net.interfaces.iter().any(|i| i.name == "lo"));
        let eth = snap
            .net
            .interfaces
            .iter()
            .find(|i| i.name == "eth0")
            .unwrap();
        assert_eq!(snap.net.rx_bytes_per_sec, eth.rx_bytes_per_sec);
        assert!(snap.net.rx_bytes_per_sec > 0);
    }

    #[test]
    fn a_host_without_cpufreq_or_sensors_still_samples() {
        let f = Fixture::new("bare");
        f.write("proc/stat", STAT_T0);
        f.write("proc/net/dev", NET_T0);
        let mut s = f.sampler();
        s.sample();
        Fixture::tick();
        f.write("proc/stat", STAT_T1);
        f.write("proc/net/dev", NET_T1);
        let snap = s.sample().unwrap();

        // No /sys at all in this fixture: absent, not zero.
        assert!(snap.cpu.freq_mhz.is_empty());
        assert_eq!(snap.cpu.mean_freq_mhz(), None);
        assert_eq!(snap.thermal.cpu_celsius, None);
        assert!(snap.thermal.sensors.is_empty());
    }

    #[test]
    fn reads_hwmon_labels_and_picks_the_package_sensor() {
        let f = Fixture::new("hwmon");
        f.write("proc/stat", STAT_T0);
        f.write("proc/net/dev", NET_T0);
        // Shaped like the real machine: a disk chip and a CPU chip.
        f.write("sys/class/hwmon/hwmon3/name", "nvme\n");
        f.write("sys/class/hwmon/hwmon3/temp1_input", "43000\n");
        f.write("sys/class/hwmon/hwmon3/temp1_label", "Composite\n");
        f.write("sys/class/hwmon/hwmon7/name", "coretemp\n");
        f.write("sys/class/hwmon/hwmon7/temp1_input", "87000\n");
        f.write("sys/class/hwmon/hwmon7/temp1_label", "Package id 0\n");
        f.write("sys/class/hwmon/hwmon7/temp2_input", "85000\n");
        f.write("sys/class/hwmon/hwmon7/temp2_label", "Core 0\n");

        let mut s = f.sampler();
        s.sample();
        Fixture::tick();
        f.write("proc/stat", STAT_T1);
        let snap = s.sample().unwrap();

        assert_eq!(snap.thermal.cpu_celsius, Some(87.0));
        assert_eq!(
            snap.thermal.cpu_source.as_deref(),
            Some("coretemp/Package id 0")
        );
        // Everything is still reported for the detail view, disk included.
        assert_eq!(snap.thermal.sensors.len(), 3);
        assert!(snap
            .thermal
            .sensors
            .iter()
            .any(|s| s.label == "nvme/Composite"));
    }

    #[test]
    fn unlabelled_sensors_fall_back_to_the_file_name() {
        let f = Fixture::new("nolabel");
        f.write("proc/stat", STAT_T0);
        f.write("proc/net/dev", NET_T0);
        f.write("sys/class/hwmon/hwmon0/name", "acpitz\n");
        f.write("sys/class/hwmon/hwmon0/temp1_input", "88000\n");

        let mut s = f.sampler();
        s.sample();
        Fixture::tick();
        f.write("proc/stat", STAT_T1);
        let snap = s.sample().unwrap();
        assert_eq!(snap.thermal.cpu_source.as_deref(), Some("acpitz/temp1"));
    }

    #[test]
    fn a_sensor_reading_exactly_zero_is_treated_as_not_fitted() {
        let f = Fixture::new("zerosensor");
        f.write("proc/stat", STAT_T0);
        f.write("proc/net/dev", NET_T0);
        // Shaped like this ThinkPad: temp4 is an unpopulated slot.
        f.write("sys/class/hwmon/hwmon6/name", "thinkpad\n");
        f.write("sys/class/hwmon/hwmon6/temp1_input", "77000\n");
        f.write("sys/class/hwmon/hwmon6/temp1_label", "CPU\n");
        f.write("sys/class/hwmon/hwmon6/temp4_input", "0\n");

        let mut s = f.sampler();
        s.sample();
        Fixture::tick();
        f.write("proc/stat", STAT_T1);
        let snap = s.sample().unwrap();

        assert_eq!(snap.thermal.sensors.len(), 1);
        assert_eq!(snap.thermal.sensors[0].label, "thinkpad/CPU");
    }

    #[test]
    fn a_sensor_exposed_twice_is_reported_once() {
        let f = Fixture::new("dupsensor");
        f.write("proc/stat", STAT_T0);
        f.write("proc/net/dev", NET_T0);
        // acpitz shows up as both an hwmon chip and a thermal zone.
        f.write("sys/class/hwmon/hwmon1/name", "acpitz\n");
        f.write("sys/class/hwmon/hwmon1/temp1_input", "88000\n");
        f.write("sys/class/thermal/thermal_zone0/type", "acpitz\n");
        f.write("sys/class/thermal/thermal_zone0/temp", "88000\n");

        let mut s = f.sampler();
        s.sample();
        Fixture::tick();
        f.write("proc/stat", STAT_T1);
        let snap = s.sample().unwrap();

        assert_eq!(snap.thermal.sensors.len(), 1, "{:?}", snap.thermal.sensors);
        assert_eq!(snap.thermal.cpu_celsius, Some(88.0));
    }

    #[test]
    fn thermal_zones_are_used_when_hwmon_has_no_cpu_chip() {
        let f = Fixture::new("zones");
        f.write("proc/stat", STAT_T0);
        f.write("proc/net/dev", NET_T0);
        f.write("sys/class/thermal/thermal_zone9/type", "x86_pkg_temp\n");
        f.write("sys/class/thermal/thermal_zone9/temp", "100000\n");

        let mut s = f.sampler();
        s.sample();
        Fixture::tick();
        f.write("proc/stat", STAT_T1);
        let snap = s.sample().unwrap();
        assert_eq!(snap.thermal.cpu_celsius, Some(100.0));
        assert_eq!(
            snap.thermal.cpu_source.as_deref(),
            Some("x86_pkg_temp/temp1")
        );
    }

    #[test]
    fn cpufreq_is_reported_in_megahertz() {
        let f = Fixture::new("freq");
        f.write("proc/stat", STAT_T0);
        f.write("proc/net/dev", NET_T0);
        f.write(
            "sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq",
            "1946266\n",
        );
        f.write(
            "sys/devices/system/cpu/cpu1/cpufreq/scaling_cur_freq",
            "2100000\n",
        );

        let mut s = f.sampler();
        s.sample();
        Fixture::tick();
        f.write("proc/stat", STAT_T1);
        let snap = s.sample().unwrap();
        assert_eq!(snap.cpu.freq_mhz, vec![1946, 2100]);
        assert_eq!(snap.cpu.mean_freq_mhz(), Some(2023));
    }

    #[test]
    fn a_core_appearing_between_samples_does_not_panic() {
        let f = Fixture::new("hotplug");
        f.write(
            "proc/stat",
            "cpu  1000 0 0 3000 0 0 0 0\ncpu0 500 0 0 1500 0 0 0 0\n",
        );
        f.write("proc/net/dev", NET_T0);
        let mut s = f.sampler();
        s.sample();
        Fixture::tick();
        // cpu1 shows up only now.
        f.write("proc/stat", STAT_T1);
        let snap = s.sample().unwrap();
        assert_eq!(snap.cpu.cores, 2);
        assert_eq!(snap.cpu.per_core_busy_pct.len(), 2);
    }

    #[test]
    fn a_missing_proc_does_not_panic() {
        let f = Fixture::new("missing");
        // No stat, no net/dev at all.
        let mut s = f.sampler();
        assert!(s.sample().is_none());
        Fixture::tick();
        let snap = s.sample();
        assert!(snap.is_some(), "second call still produces a snapshot");
        let snap = snap.unwrap();
        assert_eq!(snap.cpu.cores, 0);
    }
}
