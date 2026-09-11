//! Pure parsers for `/proc` and `/sys` contents.
//!
//! Kept free of I/O so every rule is testable against real captured text. The
//! fixtures in the tests below were taken verbatim from a running machine.

use icecc_metrics::{Load, Mem};

/// Cumulative CPU jiffies for one CPU, from a `/proc/stat` `cpu*` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    pub total: u64,
    /// `idle + iowait`: time the CPU had nothing to run.
    pub idle: u64,
}

impl CpuTimes {
    /// Busy percentage between two cumulative readings.
    ///
    /// Returns `None` when the counters did not advance, which happens on the
    /// very first sample and if two samples land in the same jiffy. Reporting
    /// 0 % there would look like an idle node.
    pub fn busy_pct_since(&self, earlier: &CpuTimes) -> Option<f32> {
        let total = self.total.checked_sub(earlier.total)?;
        let idle = self.idle.saturating_sub(earlier.idle);
        if total == 0 {
            return None;
        }
        let busy = total.saturating_sub(idle);
        Some((busy as f32 * 100.0 / total as f32).clamp(0.0, 100.0))
    }
}

/// Parse `/proc/stat` into the aggregate line and the per-core lines.
///
/// Per-core entries come back in file order, which is `cpu0..cpuN` on Linux and
/// is the order the wire format promises.
pub fn stat(text: &str) -> (Option<CpuTimes>, Vec<CpuTimes>) {
    let mut total = None;
    let mut per_core = Vec::new();

    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            // `/proc/stat` continues with intr/ctxt/btime; nothing we need.
            continue;
        };
        let (is_aggregate, fields) = match rest.split_once(char::is_whitespace) {
            // "cpu  1 2 3 …" — the aggregate line has no index.
            Some(("", f)) => (true, f),
            Some((_index, f)) => (false, f),
            None => continue,
        };

        let values: Vec<u64> = fields
            .split_whitespace()
            .filter_map(|v| v.parse().ok())
            .collect();
        // user nice system idle iowait irq softirq steal [guest guest_nice]
        if values.len() < 5 {
            continue;
        }
        // guest and guest_nice are already counted inside user and nice, so
        // summing them again would inflate the total.
        let times = CpuTimes {
            total: values.iter().take(8).sum(),
            idle: values[3] + values[4],
        };
        if is_aggregate {
            total = Some(times);
        } else {
            per_core.push(times);
        }
    }

    (total, per_core)
}

/// Parse `/proc/meminfo`. Values there are already kibibytes.
pub fn meminfo(text: &str) -> Mem {
    let mut m = Mem {
        total_kib: 0,
        available_kib: 0,
        free_kib: 0,
        buffers_kib: 0,
        cached_kib: 0,
        swap_total_kib: 0,
        swap_free_kib: 0,
    };

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        // "MemTotal:       65540720 kB"
        let Some(kib) = value.split_whitespace().next().and_then(|v| v.parse().ok()) else {
            continue;
        };
        match key {
            "MemTotal" => m.total_kib = kib,
            "MemAvailable" => m.available_kib = kib,
            "MemFree" => m.free_kib = kib,
            "Buffers" => m.buffers_kib = kib,
            "Cached" => m.cached_kib = kib,
            "SwapTotal" => m.swap_total_kib = kib,
            "SwapFree" => m.swap_free_kib = kib,
            _ => {}
        }
    }

    // MemAvailable arrived in Linux 3.14. On anything older, approximate it the
    // way the kernel does, rather than reporting the node as fully used.
    if m.available_kib == 0 && m.total_kib > 0 {
        m.available_kib = m.free_kib + m.buffers_kib + m.cached_kib;
    }
    m
}

/// Parse `/proc/loadavg`: `2.49 3.81 8.67 1/4540 541387`.
pub fn loadavg(text: &str) -> Option<Load> {
    let mut fields = text.split_whitespace();
    let one = fields.next()?.parse().ok()?;
    let five = fields.next()?.parse().ok()?;
    let fifteen = fields.next()?.parse().ok()?;

    let (runnable, total_procs) = match fields.next().and_then(|f| f.split_once('/')) {
        Some((r, t)) => (r.parse().unwrap_or(0), t.parse().unwrap_or(0)),
        None => (0, 0),
    };

    Some(Load {
        one,
        five,
        fifteen,
        runnable,
        total_procs,
    })
}

/// Parse `/proc/uptime`, whose first field is seconds since boot.
pub fn uptime_secs(text: &str) -> Option<u64> {
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .map(|s| s as u64)
}

/// Cumulative byte counters for one interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfCounters {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// Parse `/proc/net/dev`.
///
/// Loopback is kept in the list but excluded from the aggregate by
/// [`is_aggregated`]: local traffic says nothing about cluster networking.
pub fn net_dev(text: &str) -> Vec<IfCounters> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue; // the two header lines
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let values: Vec<u64> = rest
            .split_whitespace()
            .map(|v| v.parse().unwrap_or(0))
            .collect();
        // receive: bytes packets errs drop fifo frame compressed multicast
        // transmit: bytes packets errs drop fifo colls carrier compressed
        if values.len() < 9 {
            continue;
        }
        out.push(IfCounters {
            name: name.to_owned(),
            rx_bytes: values[0],
            tx_bytes: values[8],
        });
    }
    out
}

/// Whether an interface counts toward the host's aggregate throughput.
pub fn is_aggregated(name: &str) -> bool {
    name != "lo"
}

/// Bytes per second between two cumulative readings.
///
/// Counters reset when an interface is recreated, so a decrease yields 0 rather
/// than a spike from an underflow.
pub fn rate_per_sec(now: u64, earlier: u64, interval_ms: u64) -> u64 {
    if interval_ms == 0 {
        return 0;
    }
    let delta = now.saturating_sub(earlier);
    delta.saturating_mul(1000) / interval_ms
}

/// A temperature sensor as read from sysfs, before it is flattened for the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSensor {
    /// hwmon chip name (`coretemp`), or thermal zone type (`x86_pkg_temp`).
    pub chip: String,
    /// hwmon label (`Package id 0`), or the sysfs file name when unlabelled.
    pub label: String,
    pub celsius: f32,
}

impl RawSensor {
    /// `chip/label`, which is what the wire format shows so a number is never
    /// unattributed.
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.chip, self.label)
    }
}

/// Millidegrees, as every sysfs temperature file reports.
pub fn millidegrees(text: &str) -> Option<f32> {
    text.trim().parse::<i64>().ok().map(|m| m as f32 / 1000.0)
}

/// Choose the sensor that best represents CPU package temperature.
///
/// Hosts expose many sensors that disagree. On the development machine
/// `coretemp/Package id 0` read 87 °C, `thinkpad/CPU` 89 °C, `acpitz` 88 °C and
/// `x86_pkg_temp` 100 °C — so this picks by an explicit preference order and
/// the choice is reported alongside the value.
pub fn pick_cpu_sensor(sensors: &[RawSensor]) -> Option<&RawSensor> {
    // Intel/AMD package sensors, which are the authoritative ones.
    const CPU_CHIPS: [&str; 4] = ["coretemp", "k10temp", "zenpower", "cpu_thermal"];

    let package = sensors.iter().find(|s| {
        CPU_CHIPS.contains(&s.chip.as_str()) && s.label.to_ascii_lowercase().contains("package")
    });
    if package.is_some() {
        return package;
    }
    // A CPU chip without a package label: take its first reading (Core 0 or
    // the single sensor on ARM SoCs).
    if let Some(s) = sensors
        .iter()
        .find(|s| CPU_CHIPS.contains(&s.chip.as_str()))
    {
        return Some(s);
    }
    // Kernel's own package sensor, then the generic ACPI thermal zone.
    for chip in ["x86_pkg_temp", "acpitz"] {
        if let Some(s) = sensors.iter().find(|s| s.chip == chip) {
            return Some(s);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from `/proc/stat` on the development machine (12 cores).
    const STAT: &str = "\
cpu  45660458 1691011 14971004 225387152 871775 0 318197 0 0 0
cpu0 3690201 137740 1272509 18751168 70930 0 37891 0 0 0
cpu1 3834303 144967 1256622 18746519 73470 0 22695 0 0 0
cpu2 3862468 143440 1255870 18753574 70726 0 15775 0 0 0
intr 3624156516 22 9 0 0 0 0 0 0 0 0 0
ctxt 8299326114
btime 1757201234
processes 12345678
";

    /// Verbatim from `/proc/meminfo`.
    const MEMINFO: &str = "\
MemTotal:       65540720 kB
MemFree:        16456404 kB
MemAvailable:   33357408 kB
Buffers:          759784 kB
Cached:         18495264 kB
SwapCached:       127024 kB
Active:         31080352 kB
Inactive:       12262060 kB
SwapTotal:       8388604 kB
SwapFree:         151148 kB
";

    /// Verbatim from `/proc/net/dev`.
    const NET_DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 118384993  516139    0    0    0     0          0         0 118384993  516139    0    0    0     0       0          0
wlp9s0: 79968079761 68399495    0    0    0     0          0         0 18480322874 16477232    0   81    0     0       0          0
";

    #[test]
    fn stat_separates_the_aggregate_from_the_cores() {
        let (total, cores) = stat(STAT);
        let total = total.expect("aggregate line");
        // 45660458+1691011+14971004+225387152+871775+0+318197 = 288899597
        assert_eq!(total.total, 288_899_597);
        assert_eq!(total.idle, 225_387_152 + 871_775);
        assert_eq!(cores.len(), 3, "only three cpuN lines in the fixture");
        assert_eq!(cores[0].idle, 18_751_168 + 70_930);
    }

    #[test]
    fn stat_ignores_the_trailing_non_cpu_lines() {
        // "ctxt" and "processes" must not be mistaken for cores.
        let (_, cores) = stat(STAT);
        assert_eq!(cores.len(), 3);
    }

    #[test]
    fn busy_percentage_comes_from_the_delta_not_the_absolute_counters() {
        // A second of a 4-core-equivalent idle machine: 400 jiffies total,
        // 300 idle -> 25 % busy.
        let a = CpuTimes {
            total: 1000,
            idle: 800,
        };
        let b = CpuTimes {
            total: 1400,
            idle: 1100,
        };
        assert_eq!(b.busy_pct_since(&a), Some(25.0));
    }

    #[test]
    fn a_fully_busy_and_fully_idle_window_read_as_100_and_0() {
        let a = CpuTimes {
            total: 1000,
            idle: 500,
        };
        assert_eq!(
            CpuTimes {
                total: 1100,
                idle: 500
            }
            .busy_pct_since(&a),
            Some(100.0)
        );
        assert_eq!(
            CpuTimes {
                total: 1100,
                idle: 600
            }
            .busy_pct_since(&a),
            Some(0.0)
        );
    }

    #[test]
    fn a_window_with_no_elapsed_jiffies_is_unknown_not_idle() {
        let a = CpuTimes {
            total: 1000,
            idle: 800,
        };
        assert_eq!(a.busy_pct_since(&a), None);
    }

    #[test]
    fn counters_going_backwards_do_not_underflow() {
        // Can happen across a CPU hotplug or a suspend/resume.
        let later = CpuTimes {
            total: 1000,
            idle: 800,
        };
        let earlier = CpuTimes {
            total: 2000,
            idle: 1500,
        };
        assert_eq!(later.busy_pct_since(&earlier), None);
    }

    #[test]
    fn meminfo_reads_the_real_file_in_kib() {
        let m = meminfo(MEMINFO);
        assert_eq!(m.total_kib, 65_540_720);
        assert_eq!(m.available_kib, 33_357_408);
        assert_eq!(m.free_kib, 16_456_404);
        assert_eq!(m.buffers_kib, 759_784);
        assert_eq!(m.cached_kib, 18_495_264);
        assert_eq!(m.swap_total_kib, 8_388_604);
        assert_eq!(m.swap_free_kib, 151_148);
        // Cross-checked against `free -m`, which reported 64004 total MiB.
        assert_eq!(m.total_kib / 1024, 64_004);
    }

    #[test]
    fn meminfo_without_memavailable_is_approximated_not_left_at_zero() {
        let old = "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 200 kB\n";
        let m = meminfo(old);
        assert_eq!(m.available_kib, 350);
        assert_eq!(m.used_kib(), 650);
    }

    #[test]
    fn loadavg_reads_averages_and_process_counts() {
        let l = loadavg("2.49 3.81 8.67 1/4540 541387").unwrap();
        assert_eq!(l.one, 2.49);
        assert_eq!(l.five, 3.81);
        assert_eq!(l.fifteen, 8.67);
        assert_eq!(l.runnable, 1);
        assert_eq!(l.total_procs, 4540);
    }

    #[test]
    fn loadavg_survives_a_short_or_broken_line() {
        assert!(loadavg("").is_none());
        assert!(loadavg("2.49 3.81").is_none());
        // Averages present but the process field malformed: still usable.
        let l = loadavg("1.0 2.0 3.0 garbage 1").unwrap();
        assert_eq!(l.runnable, 0);
    }

    #[test]
    fn uptime_takes_the_first_field() {
        assert_eq!(uptime_secs("275290.06 2253871.58"), Some(275_290));
        assert_eq!(uptime_secs(""), None);
    }

    #[test]
    fn net_dev_reads_rx_and_tx_byte_columns() {
        let ifaces = net_dev(NET_DEV);
        assert_eq!(ifaces.len(), 2);
        assert_eq!(ifaces[0].name, "lo");
        assert_eq!(ifaces[1].name, "wlp9s0");
        assert_eq!(ifaces[1].rx_bytes, 79_968_079_761);
        assert_eq!(ifaces[1].tx_bytes, 18_480_322_874);
    }

    #[test]
    fn loopback_is_listed_but_not_aggregated() {
        assert!(!is_aggregated("lo"));
        assert!(is_aggregated("wlp9s0"));
        assert!(is_aggregated("eth0"));
    }

    #[test]
    fn rates_are_per_second_regardless_of_the_sample_window() {
        assert_eq!(rate_per_sec(2000, 1000, 1000), 1000);
        assert_eq!(rate_per_sec(2000, 1000, 500), 2000);
        assert_eq!(rate_per_sec(2000, 1000, 2000), 500);
    }

    #[test]
    fn a_counter_reset_yields_zero_not_a_spike() {
        // Interface recreated: the new counter is lower than the old one.
        assert_eq!(rate_per_sec(5, 1_000_000, 1000), 0);
        assert_eq!(rate_per_sec(1000, 0, 0), 0);
    }

    #[test]
    fn temperatures_are_millidegrees() {
        assert_eq!(millidegrees("87000"), Some(87.0));
        assert_eq!(millidegrees(" 69050\n"), Some(69.05));
        assert_eq!(millidegrees("garbage"), None);
    }

    fn sensors_from_the_dev_machine() -> Vec<RawSensor> {
        vec![
            RawSensor {
                chip: "acpitz".into(),
                label: "temp1".into(),
                celsius: 88.0,
            },
            RawSensor {
                chip: "thinkpad".into(),
                label: "CPU".into(),
                celsius: 89.0,
            },
            RawSensor {
                chip: "coretemp".into(),
                label: "Package id 0".into(),
                celsius: 87.0,
            },
            RawSensor {
                chip: "coretemp".into(),
                label: "Core 0".into(),
                celsius: 87.0,
            },
            RawSensor {
                chip: "x86_pkg_temp".into(),
                label: "temp1".into(),
                celsius: 100.0,
            },
            RawSensor {
                chip: "nvme".into(),
                label: "Composite".into(),
                celsius: 43.0,
            },
        ]
    }

    #[test]
    fn prefers_the_cpu_package_sensor_over_every_other_reading() {
        let sensors = sensors_from_the_dev_machine();
        let picked = pick_cpu_sensor(&sensors).unwrap();
        assert_eq!(picked.qualified(), "coretemp/Package id 0");
        assert_eq!(picked.celsius, 87.0);
    }

    #[test]
    fn falls_back_through_cpu_chip_then_kernel_then_acpi() {
        let mut sensors = sensors_from_the_dev_machine();

        // No package label: any coretemp reading beats the generic zones.
        sensors.retain(|s| s.label != "Package id 0");
        assert_eq!(
            pick_cpu_sensor(&sensors).unwrap().qualified(),
            "coretemp/Core 0"
        );

        sensors.retain(|s| s.chip != "coretemp");
        assert_eq!(
            pick_cpu_sensor(&sensors).unwrap().qualified(),
            "x86_pkg_temp/temp1"
        );

        sensors.retain(|s| s.chip != "x86_pkg_temp");
        assert_eq!(
            pick_cpu_sensor(&sensors).unwrap().qualified(),
            "acpitz/temp1"
        );
    }

    #[test]
    fn never_reports_a_disk_or_battery_sensor_as_the_cpu() {
        let sensors = vec![
            RawSensor {
                chip: "nvme".into(),
                label: "Composite".into(),
                celsius: 43.0,
            },
            RawSensor {
                chip: "BAT0".into(),
                label: "temp1".into(),
                celsius: 31.0,
            },
        ];
        assert!(pick_cpu_sensor(&sensors).is_none());
    }

    #[test]
    fn an_arm_soc_sensor_is_recognised() {
        let sensors = vec![RawSensor {
            chip: "cpu_thermal".into(),
            label: "temp1".into(),
            celsius: 55.0,
        }];
        assert_eq!(
            pick_cpu_sensor(&sensors).unwrap().qualified(),
            "cpu_thermal/temp1"
        );
    }

    #[test]
    fn no_sensors_at_all_is_none_not_a_panic() {
        assert!(pick_cpu_sensor(&[]).is_none());
    }
}
