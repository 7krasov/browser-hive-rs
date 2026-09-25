//! Reaps the zombies the worker inherits from browsers it killed.
//!
//! The worker is the container's PID 1, so every process whose parent dies is reparented to it —
//! and a reparented process that exits stays a zombie until its new parent waits for it. A replaced
//! browser pool kills its browser's whole process tree (`kill_browser_process_tree`), whose
//! processes are reparented to the worker as their parents die, so each replacement used to leave
//! ~9 zombies behind (measured 2026-09-25, Brave on Linux) with nobody to collect them. They hold
//! no memory, but they hold PIDs, and a long-lived pod would fill its PID limit.
//!
//! **Never reap a process headless_chrome launched.** The crate owns that `Child` and waits for it
//! itself; were we to reap it first, the crate's later `kill()` would go to whatever process has
//! reused the PID by then. So every launched PID is registered here, and a sweep is skipped while a
//! launch is in progress — until `Browser::new` returns, the new PID is not registered yet.
//!
//! ⚠️ The same rule covers any other code in the worker process: a child spawned with
//! `std::process::Command` or `tokio::process` would lose its exit status to a sweep (and its
//! `Child` would be exposed to PID reuse) unless its PID is registered here. Nothing spawns such
//! children today.
//!
//! Linux-only by nature (`/proc`); elsewhere a sweep finds nothing.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use tracing::info;

#[derive(Default)]
struct Registry {
    /// Launches whose PID is not known yet.
    launching: usize,
    /// PIDs headless_chrome launched and still owns.
    launched: HashSet<u32>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    // A panic under this lock leaves the set intact; there is no invariant to lose.
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Held for the duration of one `Browser::new`; sweeps are skipped while any is alive.
pub(crate) struct LaunchGuard(());

impl LaunchGuard {
    /// Register the PID the launch produced. `None` (no process id) registers nothing.
    pub(crate) fn launched(self, pid: Option<u32>) {
        if let Some(pid) = pid {
            registry().launched.insert(pid);
        }
        // Drop ends the launch.
    }
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        registry().launching -= 1;
    }
}

/// Announce a launch. Call before `Browser::new` and keep the guard until its PID is registered.
pub(crate) fn begin_launch() -> LaunchGuard {
    registry().launching += 1;
    LaunchGuard(())
}

/// Reap every zombie child of the worker that headless_chrome did not launch; returns how many.
///
/// Blocking (reads `/proc`), but brief. Holds the registry lock for the whole sweep, so no launch
/// can start between the check and the reaping.
pub(crate) fn reap_orphan_zombies() -> usize {
    let mut registry = registry();
    if registry.launching > 0 {
        return 0;
    }

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let me = std::process::id();
    let mut alive = HashSet::new();
    let mut reaped = 0;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        alive.insert(pid);
        let Some((state, ppid)) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| crate::browser_resources::parse_state_and_ppid(&stat))
        else {
            continue;
        };
        if state == 'Z' && ppid == me && !registry.launched.contains(&pid) && reap(pid) {
            reaped += 1;
        }
    }

    // headless_chrome has reaped these itself once they are gone from /proc.
    registry.launched.retain(|pid| alive.contains(pid));

    if reaped > 0 {
        info!(
            "Reaped {} zombie process(es) left by killed browsers",
            reaped
        );
    }
    reaped
}

/// Collect one exited child without blocking.
fn reap(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    let mut status = 0;
    // SAFETY: waitpid(2) with WNOHANG writes only to `status`, which outlives the call.
    unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) == pid }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::Duration;

    /// An orphan that exits is reaped; a registered launch is not. Needs the test process to be a
    /// subreaper, standing in for the worker being PID 1.
    #[test]
    fn reaps_inherited_zombies_but_not_launched_children() {
        // SAFETY: prctl(PR_SET_CHILD_SUBREAPER) takes plain integers.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0
        );

        // The shell exits at once, orphaning `sleep`, which is reparented to us and exits later.
        let mut shell = std::process::Command::new("sh")
            .args(["-c", "sleep 0.2 & exit 0"])
            .spawn()
            .unwrap();
        shell.wait().unwrap();

        // A launched child that has already exited must survive the sweep for its owner.
        let mut launched = std::process::Command::new("true").spawn().unwrap();
        begin_launch().launched(Some(launched.id()));

        std::thread::sleep(Duration::from_millis(600));
        assert!(
            reap_orphan_zombies() >= 1,
            "the orphaned sleep must be reaped"
        );
        assert!(
            launched.wait().unwrap().success(),
            "the owner must still collect its child"
        );
    }
}
