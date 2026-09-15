//! What the browser is made of right now, for the metrics endpoint: Chromium processes by type with
//! their memory, and CDP targets by type.
//!
//! A worker's memory limit is spent almost entirely by the browser's child processes, and the
//! container's own memory figure is only their sum — it cannot say whether a pod is dying of one
//! bloated renderer, of many small ones (out-of-process iframes), or of something that outlived the
//! page it served. Before these gauges the only way to tell was `kubectl exec` and `ps`.
//!
//! **Memory is PSS, not RSS.** Chromium forks renderers from a zygote, so they share most of their
//! pages; summing RSS counts those pages once per process and overstates the total several times
//! over. PSS divides every shared page between the processes mapping it, so the per-type sums add
//! up to what the processes really cost.
//!
//! Everything here is Linux-only by nature (`/proc`); elsewhere the collectors return `None` and
//! the gauges are left out of the exposition rather than reported as zero.

use crate::browser_cdp::BrowserCdpClient;
use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, TryLockError};
use std::time::Duration;

/// Label values of the process gauges. A closed set, so a Chromium release that adds a process
/// type cannot mint new series; anything unrecognised is `other`.
pub const PROCESS_TYPES: [&str; 9] = [
    "browser",
    "renderer",
    "extension",
    "gpu",
    "zygote",
    "network",
    "storage",
    "utility",
    "other",
];

/// Label values of the target gauge, with the same closed-set rule as [`PROCESS_TYPES`].
///
/// `browser_ui` is the browser's own internal pages. It gets a label because Chrome reported two
/// more of them for one new context and tab, so it is a candidate for growing with contexts.
pub const TARGET_TYPES: [&str; 7] = [
    "page",
    "iframe",
    "worker",
    "shared_worker",
    "service_worker",
    "browser_ui",
    "other",
];

/// Bound on each browser-level call of the probe, and on each step of its connection.
///
/// `Target.getTargets` answers in milliseconds; the default bound of the client is sized for
/// context creation on the request path and would let one scrape wait tens of seconds.
const PROBE_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Processes of one type.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProcessGroup {
    pub count: u64,
    pub pss_bytes: u64,
    /// The largest single process, which separates "one bloated renderer" from "many small ones"
    /// when the sum alone is the same.
    pub max_pss_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerProcess {
    pub resident_bytes: u64,
    pub threads: u64,
}

#[derive(Debug, Default)]
pub struct ProcessSnapshot {
    /// Browser processes by type, keyed by values of [`PROCESS_TYPES`]. `None` when `/proc` could
    /// not be read.
    pub browser: Option<HashMap<&'static str, ProcessGroup>>,
    pub worker: Option<WorkerProcess>,
}

#[derive(Debug)]
pub struct TargetSnapshot {
    /// Targets by type, keyed by values of [`TARGET_TYPES`].
    pub targets: HashMap<&'static str, u64>,
    /// Browser contexts other than the default one.
    pub contexts: u64,
}

/// Read the worker's own process and every process descending from it.
///
/// Descendants rather than "every Chromium process in the container": the worker launches the
/// browser, so its subtree is exactly the browser — and a previous browser process that was never
/// reaped still counts, which is the point (it shows up as `browser` above 1). What this cannot see
/// is a process reparented away from the worker, which happens only when the worker is not the
/// container's PID 1.
///
/// Blocking: reads one `smaps_rollup` per browser process, which walks that process's page tables.
pub fn collect_processes() -> ProcessSnapshot {
    ProcessSnapshot {
        browser: collect_browser_processes(),
        worker: std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| parse_worker_status(&status)),
    }
}

fn collect_browser_processes() -> Option<HashMap<&'static str, ProcessGroup>> {
    let parents: Vec<(u32, u32)> = std::fs::read_dir("/proc")
        .ok()?
        .filter_map(|entry| {
            let pid: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            Some((pid, parse_ppid(&stat)?))
        })
        .collect();

    let mut groups = HashMap::new();
    for pid in descendants(std::process::id(), &parents) {
        // A process can exit between the listing and these reads; it is simply not counted.
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        // Zombies and kernel threads have no command line and no memory of their own.
        if cmdline.is_empty() {
            continue;
        }
        let pss_bytes = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
            .ok()
            .and_then(|rollup| parse_status_number(&rollup, "Pss:"))
            .map_or(0, |kib| kib * 1024);

        let group: &mut ProcessGroup = groups.entry(classify_process(&cmdline)).or_default();
        group.count += 1;
        group.pss_bytes += pss_bytes;
        group.max_pss_bytes = group.max_pss_bytes.max(pss_bytes);
    }
    Some(groups)
}

/// Parent pid from `/proc/<pid>/stat`. The command name in parentheses may itself contain spaces
/// and parentheses, so fields are counted from the *last* `)`.
fn parse_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After the name: state, then ppid.
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Every pid below `root` in the process tree, `root` itself excluded.
fn descendants(root: u32, parents: &[(u32, u32)]) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, ppid) in parents {
        children.entry(ppid).or_default().push(pid);
    }

    let mut found = Vec::new();
    let mut seen = HashSet::from([root]);
    let mut queue = VecDeque::from([root]);
    while let Some(pid) = queue.pop_front() {
        for &child in children.get(&pid).into_iter().flatten() {
            if seen.insert(child) {
                found.push(child);
                queue.push_back(child);
            }
        }
    }
    found
}

/// Map a `/proc/<pid>/cmdline` to a value of [`PROCESS_TYPES`].
///
/// Chromium marks every child with `--type=`; the main process is the one without it (a wrapper
/// script that does not `exec` would be counted as `browser` as well). Utility processes are told
/// apart by `--utility-sub-type=`, of which only the two that hold real memory get their own label.
///
/// Arguments are split on NUL **and** whitespace: on Linux Chromium rewrites its process title,
/// after which `cmdline` is one space-joined string instead of NUL-separated arguments. Splitting on
/// NUL alone classified every process of a production pod as `browser`. A flag value containing
/// spaces (a user agent) is split too, which is harmless: only whole `--type=`-style tokens match.
fn classify_process(cmdline: &[u8]) -> &'static str {
    let args: Vec<&str> = cmdline
        .split(|&byte| byte == 0 || byte.is_ascii_whitespace())
        .filter(|arg| !arg.is_empty())
        .filter_map(|arg| std::str::from_utf8(arg).ok())
        .collect();
    let value_of = |flag: &str| args.iter().find_map(|arg| arg.strip_prefix(flag));

    match value_of("--type=") {
        None => "browser",
        Some("renderer") if args.contains(&"--extension-process") => "extension",
        Some("renderer") => "renderer",
        Some("gpu-process") => "gpu",
        Some("zygote") => "zygote",
        Some("utility") => match value_of("--utility-sub-type=") {
            Some(sub) if sub.starts_with("network.") => "network",
            Some(sub) if sub.starts_with("storage.") => "storage",
            _ => "utility",
        },
        Some(_) => "other",
    }
}

fn parse_worker_status(status: &str) -> Option<WorkerProcess> {
    Some(WorkerProcess {
        resident_bytes: parse_status_number(status, "VmRSS:")? * 1024,
        threads: parse_status_number(status, "Threads:")?,
    })
}

/// The first number on the line starting with `field` (`/proc` sizes are in KiB).
fn parse_status_number(text: &str, field: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(field))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Map a CDP `TargetInfo.type` to a value of [`TARGET_TYPES`].
fn classify_target(target_type: &str) -> &'static str {
    TARGET_TYPES
        .iter()
        .find(|&&known| known == target_type)
        .copied()
        .unwrap_or("other")
}

fn count_target_types<'a>(types: impl IntoIterator<Item = &'a str>) -> HashMap<&'static str, u64> {
    let mut counts = HashMap::new();
    for target_type in types {
        *counts.entry(classify_target(target_type)).or_default() += 1;
    }
    counts
}

/// Counts targets and browser contexts over a browser-level CDP connection of its own.
///
/// A connection of its own, not the pool's: `BrowserCdpClient` serialises calls per socket, and a
/// scrape waiting on a wedged browser must never hold the lock that context creation needs. It is
/// opened lazily and replaced when the browser is (a recreated pool has a new endpoint).
#[derive(Default)]
pub struct BrowserTargetProbe {
    client: Mutex<Option<(String, BrowserCdpClient)>>,
}

impl BrowserTargetProbe {
    /// Blocking. Fails immediately when the previous probe is still running, so a browser that
    /// stopped answering ties up at most one blocking thread however often it is scraped.
    pub fn snapshot(&self, ws_url: &str) -> Result<TargetSnapshot> {
        let mut slot = match self.client.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => bail!("the previous target probe is still running"),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };

        let (_, client) = match slot.take() {
            Some((url, client)) if url == ws_url => slot.insert((url, client)),
            _ => slot.insert((
                ws_url.to_string(),
                BrowserCdpClient::connect_with_timeout(ws_url, PROBE_CALL_TIMEOUT)?,
            )),
        };

        Ok(TargetSnapshot {
            targets: count_target_types(
                client
                    .targets()?
                    .iter()
                    .map(|target| target.target_type.as_str()),
            ),
            contexts: client.browser_context_count()? as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmdline(args: &[&str]) -> Vec<u8> {
        args.join("\0").into_bytes()
    }

    #[test]
    fn ppid_is_read_after_the_last_parenthesis() {
        assert_eq!(parse_ppid("42 (brave) S 7 42 42 0"), Some(7));
        assert_eq!(parse_ppid("42 (Web Content (x)) R 9 42 42 0"), Some(9));
        assert_eq!(parse_ppid("garbage"), None);
    }

    #[test]
    fn descendants_cover_the_whole_subtree_and_nothing_else() {
        // 1 → 10 (browser) → 11 (zygote) → 12 (renderer); 20 is unrelated.
        let parents = [(10, 1), (11, 10), (12, 11), (20, 0), (13, 10)];
        let mut found = descendants(1, &parents);
        found.sort_unstable();
        assert_eq!(found, vec![10, 11, 12, 13]);
        assert!(descendants(20, &parents).is_empty());
    }

    #[test]
    fn processes_are_classified_by_type_flags() {
        let cases = [
            (vec!["/usr/bin/brave", "--headless"], "browser"),
            (vec!["brave", "--type=renderer"], "renderer"),
            (
                vec!["brave", "--type=renderer", "--extension-process"],
                "extension",
            ),
            (vec!["brave", "--type=gpu-process"], "gpu"),
            (
                vec!["brave", "--type=zygote", "--no-zygote-sandbox"],
                "zygote",
            ),
            (
                vec![
                    "brave",
                    "--type=utility",
                    "--utility-sub-type=network.mojom.NetworkService",
                ],
                "network",
            ),
            (
                vec![
                    "brave",
                    "--type=utility",
                    "--utility-sub-type=storage.mojom.StorageService",
                ],
                "storage",
            ),
            (
                vec![
                    "brave",
                    "--type=utility",
                    "--utility-sub-type=audio.mojom.AudioService",
                ],
                "utility",
            ),
            (vec!["brave", "--type=crashpad-handler"], "other"),
        ];
        for (args, expected) in cases {
            assert_eq!(classify_process(&cmdline(&args)), expected, "{args:?}");
            assert!(PROCESS_TYPES.contains(&expected));
        }
    }

    #[test]
    fn rewritten_process_titles_are_classified_too() {
        // After Chromium rewrites its title, cmdline is one space-joined, NUL-terminated string.
        let title = |args: &str| format!("{args}\0").into_bytes();
        assert_eq!(
            classify_process(&title(
                "/opt/brave/brave --type=renderer --user-agent=Mozilla/5.0 (X11; Linux x86_64) --lang=en"
            )),
            "renderer"
        );
        assert_eq!(
            classify_process(&title(
                "/opt/brave/brave --type=utility --utility-sub-type=network.mojom.NetworkService"
            )),
            "network"
        );
        assert_eq!(
            classify_process(&title("/opt/brave/brave --headless --no-sandbox")),
            "browser"
        );
    }

    #[test]
    fn status_numbers_are_parsed_by_field() {
        let status = "Name:\tworker\nVmRSS:\t   84704 kB\nThreads:\t12\n";
        assert_eq!(
            parse_worker_status(status),
            Some(WorkerProcess {
                resident_bytes: 84704 * 1024,
                threads: 12,
            })
        );
        let rollup = "55d0-7ffd ---p 00000000 00:00 0 [rollup]\nRss:  900 kB\nPss:  412 kB\n";
        assert_eq!(parse_status_number(rollup, "Pss:"), Some(412));
        assert_eq!(parse_status_number(rollup, "Swap:"), None);
    }

    #[test]
    fn unknown_target_types_collapse_to_other() {
        let counts = count_target_types(["page", "iframe", "iframe", "webview", "auction_worklet"]);
        assert_eq!(counts.get("page"), Some(&1));
        assert_eq!(counts.get("iframe"), Some(&2));
        assert_eq!(counts.get("other"), Some(&2));
    }
}
