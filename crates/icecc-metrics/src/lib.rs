//! The node metrics wire format, shared by `icecream-watcher-agent` (producer) and
//! `icecream-watcher` (consumer).
//!
//! Everything the scheduler cannot tell us lives here: CPU utilisation,
//! per-core load, real memory totals, swap, temperature, frequency, uptime and
//! network throughput. See `ARCHITECTURE.md` §3 for why each field needs an
//! agent rather than the scheduler.
//!
//! **Units are part of every field name.** That is deliberate: Icecream's own
//! `FreeMem` field is documented as one unit but does not agree across
//! platforms (ARCHITECTURE.md §9), and the only real defence is to make the
//! unit impossible to misread at the point of use.

pub mod client;

use serde::{Deserialize, Serialize};

/// Default TCP port for the agent. Chosen to avoid Icecream's own ports —
/// scheduler 8765, scheduler text interface 8766, `iceccd` 10245 — and the
/// 8767–8769 range the icecream test suite uses.
pub const DEFAULT_AGENT_PORT: u16 = 9765;

/// The one endpoint that matters. `GET`, no parameters.
pub const METRICS_PATH: &str = "/metrics";

/// Liveness endpoint, for orchestration that wants something cheaper.
pub const HEALTH_PATH: &str = "/healthz";

/// Bumped only on an incompatible change. The consumer refuses a snapshot it
/// does not understand rather than silently reading fields as zero.
pub const SCHEMA_VERSION: u32 = 1;

/// One agent's view of its host at a moment in time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub schema: u32,
    pub agent_version: String,
    /// The agent's own idea of its hostname. Compared against the name the
    /// scheduler reports, so reaching the wrong host through NAT is visible.
    pub hostname: String,
    /// Every non-loopback address the agent found, so a multi-homed node can be
    /// matched however the scheduler happens to know it.
    pub addresses: Vec<String>,
    pub uptime_secs: u64,
    /// Wall clock at sampling. Only used to show the agent's own clock in the
    /// detail view; staleness is judged by the monitor's clock, since the two
    /// machines' clocks cannot be assumed to agree.
    pub sampled_unix_ms: u64,
    /// The window the rate fields were averaged over.
    pub sample_interval_ms: u64,
    pub cpu: Cpu,
    pub mem: Mem,
    pub load: Load,
    pub thermal: Thermal,
    pub net: Net,
}

impl Snapshot {
    /// True when this snapshot's schema is one we can read.
    pub fn schema_supported(&self) -> bool {
        self.schema == SCHEMA_VERSION
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cpu {
    pub cores: usize,
    /// Non-idle time over the sample window, 0..100.
    pub total_busy_pct: f32,
    /// Same, per core, in `/proc/stat` order.
    pub per_core_busy_pct: Vec<f32>,
    /// Current frequency per core, where the kernel exposes it. Empty on
    /// systems without `cpufreq`.
    pub freq_mhz: Vec<u32>,
}

impl Cpu {
    /// Mean current frequency, or `None` where `cpufreq` is unavailable.
    pub fn mean_freq_mhz(&self) -> Option<u32> {
        if self.freq_mhz.is_empty() {
            return None;
        }
        let sum: u64 = self.freq_mhz.iter().map(|&f| f as u64).sum();
        Some((sum / self.freq_mhz.len() as u64) as u32)
    }
}

/// Memory, straight from `/proc/meminfo`, in the kibibytes that file uses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mem {
    pub total_kib: u64,
    /// `MemAvailable`: the kernel's own estimate of what a new workload can
    /// get. This is the number to drive a "memory constrained" judgement.
    pub available_kib: u64,
    pub free_kib: u64,
    pub buffers_kib: u64,
    pub cached_kib: u64,
    pub swap_total_kib: u64,
    pub swap_free_kib: u64,
}

impl Mem {
    /// What `free` calls used: everything not available for new work.
    pub fn used_kib(&self) -> u64 {
        self.total_kib.saturating_sub(self.available_kib)
    }

    pub fn used_pct(&self) -> Option<f32> {
        (self.total_kib > 0).then(|| self.used_kib() as f32 * 100.0 / self.total_kib as f32)
    }

    pub fn swap_used_kib(&self) -> u64 {
        self.swap_total_kib.saturating_sub(self.swap_free_kib)
    }

    /// `None` when the host has no swap, which is different from 0 % used.
    pub fn swap_used_pct(&self) -> Option<f32> {
        (self.swap_total_kib > 0)
            .then(|| self.swap_used_kib() as f32 * 100.0 / self.swap_total_kib as f32)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Load {
    pub one: f32,
    pub five: f32,
    pub fifteen: f32,
    pub runnable: u32,
    pub total_procs: u32,
}

impl Load {
    /// Load average per core: 1.0 means "as many runnable tasks as cores",
    /// which is what makes load comparable across differently sized nodes.
    pub fn per_core(&self, cores: usize) -> Option<f32> {
        (cores > 0).then(|| self.one / cores as f32)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Thermal {
    /// The CPU package temperature, when a sensor for it could be identified.
    pub cpu_celsius: Option<f32>,
    /// Which sensor `cpu_celsius` came from, e.g. `coretemp/Package id 0`.
    /// Named because hosts disagree wildly between sensors — on the development
    /// machine `acpitz` read 88 °C while `coretemp` package read 87 °C and
    /// `x86_pkg_temp` read 100 °C — so an unattributed number is not evidence.
    pub cpu_source: Option<String>,
    /// Everything found, for the detail view.
    pub sensors: Vec<Sensor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sensor {
    pub label: String,
    pub celsius: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Net {
    pub rx_bytes_per_sec: u64,
    pub tx_bytes_per_sec: u64,
    pub interfaces: Vec<Interface>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Interface {
    pub name: String,
    pub rx_bytes_per_sec: u64,
    pub tx_bytes_per_sec: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            schema: SCHEMA_VERSION,
            agent_version: "0.1.0".into(),
            hostname: "build01".into(),
            addresses: vec!["10.0.0.11".into()],
            uptime_secs: 275_290,
            sampled_unix_ms: 1_700_000_000_000,
            sample_interval_ms: 1000,
            cpu: Cpu {
                cores: 12,
                total_busy_pct: 82.5,
                per_core_busy_pct: vec![90.0; 12],
                freq_mhz: vec![1946; 12],
            },
            mem: Mem {
                total_kib: 65_540_720,
                available_kib: 33_357_408,
                free_kib: 16_456_404,
                buffers_kib: 759_784,
                cached_kib: 18_495_264,
                swap_total_kib: 8_388_604,
                swap_free_kib: 151_148,
            },
            load: Load {
                one: 2.49,
                five: 3.81,
                fifteen: 8.67,
                runnable: 1,
                total_procs: 4540,
            },
            thermal: Thermal {
                cpu_celsius: Some(87.0),
                cpu_source: Some("coretemp/Package id 0".into()),
                sensors: vec![Sensor {
                    label: "coretemp/Package id 0".into(),
                    celsius: 87.0,
                }],
            },
            net: Net {
                rx_bytes_per_sec: 1_200_000,
                tx_bytes_per_sec: 4_800_000,
                interfaces: vec![Interface {
                    name: "wlp9s0".into(),
                    rx_bytes_per_sec: 1_200_000,
                    tx_bytes_per_sec: 4_800_000,
                }],
            },
        }
    }

    #[test]
    fn round_trips_through_json() {
        let s = sample();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&json).unwrap(), s);
    }

    #[test]
    fn memory_derivations_match_the_real_meminfo_this_was_taken_from() {
        let m = sample().mem;
        // total 65540720 KiB, available 33357408 KiB -> used 32183312 KiB (~30.7 GiB)
        assert_eq!(m.used_kib(), 32_183_312);
        let pct = m.used_pct().unwrap();
        assert!((pct - 49.1).abs() < 0.2, "used {pct}%");
        assert_eq!(m.swap_used_kib(), 8_237_456);
        assert!(m.swap_used_pct().unwrap() > 98.0);
    }

    #[test]
    fn no_swap_is_unknown_not_zero_percent() {
        let mut m = sample().mem;
        m.swap_total_kib = 0;
        m.swap_free_kib = 0;
        assert_eq!(m.swap_used_pct(), None);
        assert_eq!(m.swap_used_kib(), 0);
    }

    #[test]
    fn empty_memory_totals_do_not_divide_by_zero() {
        let m = Mem {
            total_kib: 0,
            available_kib: 0,
            free_kib: 0,
            buffers_kib: 0,
            cached_kib: 0,
            swap_total_kib: 0,
            swap_free_kib: 0,
        };
        assert_eq!(m.used_pct(), None);
    }

    #[test]
    fn available_above_total_cannot_produce_a_negative_used() {
        // Defensive: a bad agent or a weird kernel must not underflow.
        let mut m = sample().mem;
        m.available_kib = m.total_kib + 1000;
        assert_eq!(m.used_kib(), 0);
    }

    #[test]
    fn load_per_core_makes_nodes_comparable() {
        let l = sample().load;
        assert!((l.per_core(12).unwrap() - 0.2075).abs() < 0.001);
        assert_eq!(l.per_core(0), None);
    }

    #[test]
    fn frequency_is_absent_rather_than_zero_without_cpufreq() {
        let mut c = sample().cpu;
        assert_eq!(c.mean_freq_mhz(), Some(1946));
        c.freq_mhz.clear();
        assert_eq!(c.mean_freq_mhz(), None);
    }

    #[test]
    fn an_unknown_schema_is_rejected_rather_than_read_as_zeroes() {
        let mut s = sample();
        s.schema = SCHEMA_VERSION + 1;
        assert!(!s.schema_supported());
    }

    #[test]
    fn missing_fields_fail_to_deserialise_instead_of_defaulting() {
        // Silent defaults would render a node as 0% CPU rather than as broken.
        let err = serde_json::from_str::<Snapshot>(r#"{"schema":1}"#);
        assert!(err.is_err());
    }
}
