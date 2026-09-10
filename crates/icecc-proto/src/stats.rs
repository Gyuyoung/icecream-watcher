//! Parser for the `MON_STATS` `statmsg` blob.
//!
//! Built by `scheduler.cpp:handle_monitor_stats()` as newline-separated
//! `Key:Value` lines. **Every record is partial**, and which fields appear
//! depends on why it was sent:
//!
//! | Trigger | Fields |
//! |---|---|
//! | login replay / node join | Name, IP, MaxJobs, NoRemote, Platform, Version, Features, Speed, Load |
//! | stats update from a node | the above **plus** LoadAvg1/5/10, FreeMem |
//! | node leaving | `State:Offline` and nothing else |
//!
//! So a record must be *merged* into whatever is already known about a node,
//! never used to replace it. See [`StatsRecord::merge_into`].

/// A partial node record. `None` means "this message said nothing about that
/// field", which is not the same as "the value is zero".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatsRecord {
    pub name: Option<String>,
    pub ip: Option<String>,
    /// Absolute value of `MaxJobs`. See [`Self::suspect`] for the sign.
    pub max_jobs: Option<u32>,
    /// The scheduler negates `MaxJobs` while a node is being pinged and has not
    /// answered yet ("better not give it away", `scheduler.cpp:1020`). A
    /// negative value therefore means the scheduler doubts the node is alive.
    pub suspect: Option<bool>,
    pub no_remote: Option<bool>,
    pub platform: Option<String>,
    /// Node's maximum remote protocol version.
    pub protocol: Option<u32>,
    pub features: Option<String>,
    /// `server_speed()`: output bytes per user-second. **Zero until the node has
    /// actually compiled something**, so a fresh cluster reports 0 everywhere.
    pub speed: Option<f64>,
    /// Composite 0..1000 scheduling weight: `max(1000 - idle, memory_fillgrade)`,
    /// forced to 1000 when the node is low on disk. **Not CPU utilisation.**
    pub load: Option<u32>,
    pub load_avg_1: Option<f64>,
    pub load_avg_5: Option<f64>,
    pub load_avg_10: Option<f64>,
    /// MiB of *available* memory (MemFree + Buffers + Cached). There is no
    /// `MemTotal` in the protocol, so this cannot be turned into a percentage.
    pub free_mem_mib: Option<u64>,
    /// `State:Offline` — the node is gone.
    pub offline: bool,
}

impl StatsRecord {
    pub fn parse(blob: &str) -> Self {
        let mut rec = Self::default();
        for line in blob.lines() {
            let line = line.trim_end_matches('\r');
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            match key {
                "Name" => rec.name = Some(value.to_owned()),
                "IP" => rec.ip = Some(value.to_owned()),
                "MaxJobs" => {
                    if let Ok(v) = value.trim().parse::<i64>() {
                        rec.max_jobs = Some(v.unsigned_abs() as u32);
                        rec.suspect = Some(v < 0);
                    }
                }
                "NoRemote" => rec.no_remote = Some(value.eq_ignore_ascii_case("true")),
                "Platform" => rec.platform = Some(value.to_owned()),
                "Version" => rec.protocol = value.trim().parse().ok(),
                "Features" => rec.features = Some(value.to_owned()),
                "Speed" => rec.speed = value.trim().parse().ok(),
                "Load" => rec.load = value.trim().parse().ok(),
                "LoadAvg1" => rec.load_avg_1 = milli(value),
                "LoadAvg5" => rec.load_avg_5 = milli(value),
                "LoadAvg10" => rec.load_avg_10 = milli(value),
                "FreeMem" => rec.free_mem_mib = value.trim().parse().ok(),
                "State" => rec.offline = value.eq_ignore_ascii_case("offline"),
                _ => {}
            }
        }
        rec
    }

    /// True when this record carries live resource data. The login replay does
    /// not, which is why a monitor cannot rely on it for a first paint.
    pub fn has_resource_fields(&self) -> bool {
        self.load_avg_1.is_some() || self.free_mem_mib.is_some()
    }

    /// Apply every field this record actually carries onto `dst`, leaving the
    /// rest of `dst` untouched.
    pub fn merge_into(&self, dst: &mut StatsRecord) {
        macro_rules! set {
            ($($f:ident),+) => { $( if self.$f.is_some() { dst.$f = self.$f.clone(); } )+ };
        }
        set!(
            name,
            ip,
            max_jobs,
            suspect,
            no_remote,
            platform,
            protocol,
            features,
            speed,
            load,
            load_avg_1,
            load_avg_5,
            load_avg_10,
            free_mem_mib
        );
        if self.offline {
            dst.offline = true;
        }
    }
}

/// `LoadAvg*` is the load average multiplied by 1000.
fn milli(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok().map(|v| v / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from a live scheduler 1.4 at monitor login.
    const LOGIN_REPLAY: &str = "Name:gyuyoung-ThinkPad-P1\nIP:127.0.0.1\nMaxJobs:4\n\
                                NoRemote:true\nPlatform:x86_64\nVersion:43\n\
                                Features:env_xz env_zstd\nSpeed:0.000000\nLoad:426\n";

    /// Captured verbatim from the same scheduler while the node was loaded.
    const LOADED_UPDATE: &str = "Name:gyuyoung-ThinkPad-P1\nIP:127.0.0.1\nMaxJobs:4\n\
                                 NoRemote:true\nPlatform:x86_64\nVersion:43\n\
                                 Features:env_xz env_zstd\nSpeed:0.000000\nLoad:839\n\
                                 LoadAvg1:2197\nLoadAvg5:1801\nLoadAvg10:1503\nFreeMem:36732\n";

    #[test]
    fn parses_the_live_login_replay() {
        let r = StatsRecord::parse(LOGIN_REPLAY);
        assert_eq!(r.name.as_deref(), Some("gyuyoung-ThinkPad-P1"));
        assert_eq!(r.ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(r.max_jobs, Some(4));
        assert_eq!(r.suspect, Some(false));
        assert_eq!(r.no_remote, Some(true));
        assert_eq!(r.platform.as_deref(), Some("x86_64"));
        assert_eq!(r.protocol, Some(43));
        assert_eq!(r.features.as_deref(), Some("env_xz env_zstd"));
        assert_eq!(r.speed, Some(0.0));
        assert_eq!(r.load, Some(426));
        // The replay carries no live resource data at all.
        assert_eq!(r.load_avg_1, None);
        assert_eq!(r.free_mem_mib, None);
        assert!(!r.has_resource_fields());
        assert!(!r.offline);
    }

    #[test]
    fn parses_the_live_loaded_update_with_units_applied() {
        let r = StatsRecord::parse(LOADED_UPDATE);
        assert_eq!(r.load, Some(839));
        assert_eq!(r.load_avg_1, Some(2.197));
        assert_eq!(r.load_avg_5, Some(1.801));
        assert_eq!(r.load_avg_10, Some(1.503));
        // MiB available; `free -m` reported 36566 on the same host.
        assert_eq!(r.free_mem_mib, Some(36732));
        assert!(r.has_resource_fields());
    }

    #[test]
    fn offline_record_carries_only_the_state() {
        let r = StatsRecord::parse("State:Offline\n");
        assert!(r.offline);
        assert_eq!(r.name, None);
        assert_eq!(r.load, None);
    }

    #[test]
    fn merging_an_offline_record_keeps_what_we_already_knew() {
        let mut node = StatsRecord::parse(LOADED_UPDATE);
        StatsRecord::parse("State:Offline\n").merge_into(&mut node);
        assert!(node.offline);
        // Identity and last known values must survive, or the row goes blank
        // exactly when the user wants to see which node died.
        assert_eq!(node.name.as_deref(), Some("gyuyoung-ThinkPad-P1"));
        assert_eq!(node.load, Some(839));
    }

    #[test]
    fn merging_a_login_replay_does_not_erase_resource_fields() {
        let mut node = StatsRecord::parse(LOADED_UPDATE);
        StatsRecord::parse(LOGIN_REPLAY).merge_into(&mut node);
        assert_eq!(node.load, Some(426)); // replay does carry Load
        assert_eq!(node.load_avg_1, Some(2.197)); // but must not clear these
        assert_eq!(node.free_mem_mib, Some(36732));
    }

    #[test]
    fn negative_max_jobs_marks_a_node_the_scheduler_doubts() {
        let r = StatsRecord::parse("Name:build01\nMaxJobs:-16\n");
        assert_eq!(r.max_jobs, Some(16));
        assert_eq!(r.suspect, Some(true));
    }

    #[test]
    fn unknown_keys_and_junk_lines_are_ignored() {
        let r = StatsRecord::parse("Name:build01\nSomethingNew:42\nno-colon-here\n\n");
        assert_eq!(r.name.as_deref(), Some("build01"));
    }

    #[test]
    fn values_containing_colons_survive() {
        let r = StatsRecord::parse("IP:fe80::1\n");
        assert_eq!(r.ip.as_deref(), Some("fe80::1"));
    }
}
