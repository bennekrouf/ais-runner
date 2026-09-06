//! Cross-platform "who owns this TCP port".
//!
//! When :7071 (Logic Apps func) or :7072 (Java func) is already bound, we want
//! to know whether it's a *stale instance of the current project* (safe to
//! reclaim) or *a different project's host* (killing it would disrupt the user's
//! other work). `lsof` is Unix-only, so Windows uses `netstat` + `wmic`.

#[derive(Debug, Clone)]
pub struct PortOwner {
    pub pid: u32,
    /// Best-effort identity of the owning process: its working directory (Unix)
    /// or full command line (Windows). Used to recognise a foreign project.
    pub detail: String,
}

/// The process listening on `port`, if any. Best-effort — returns None when
/// nothing is listening or the platform's tools aren't available.
pub fn owner(port: u16) -> Option<PortOwner> {
    #[cfg(windows)]
    {
        owner_windows(port)
    }
    #[cfg(not(windows))]
    {
        owner_unix(port)
    }
}

#[cfg(not(windows))]
fn owner_unix(port: u16) -> Option<PortOwner> {
    use std::process::Command;
    let pid_out = Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    let pid: u32 = String::from_utf8_lossy(&pid_out.stdout)
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    // Working directory of that pid.
    let cwd = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .find(|l| l.starts_with('n'))
                .map(|l| l[1..].to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    Some(PortOwner { pid, detail: cwd })
}

#[cfg(windows)]
fn owner_windows(port: u16) -> Option<PortOwner> {
    use std::process::Command;
    // netstat -ano: find the LISTENING row for :port; PID is the last column.
    let ns = Command::new("cmd")
        .args([
            "/c",
            &format!("netstat -ano -p tcp | findstr LISTENING | findstr :{port}"),
        ])
        .output()
        .ok()?;
    let pid: u32 = String::from_utf8_lossy(&ns.stdout)
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().last())
        .and_then(|p| p.parse().ok())?;
    // Command line of that pid (contains the project path / --script-root arg).
    let detail = Command::new("cmd")
        .args([
            "/c",
            &format!("wmic process where processid={pid} get commandline /value"),
        ])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .find_map(|l| l.strip_prefix("CommandLine="))
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    Some(PortOwner { pid, detail })
}

/// True when the port owner looks like it belongs to `project_dir` — i.e. a
/// stale instance of the same project, safe to reclaim. A mismatch means a
/// *foreign* project holds the port.
pub fn belongs_to(owner: &PortOwner, project_dir: &str) -> bool {
    let needle = project_dir.trim_end_matches(['/', '\\']);
    !needle.is_empty() && !owner.detail.is_empty() && owner.detail.contains(needle)
}

/// Force-kill whatever is *listening* on `port`, and return its pid.
///
/// Safety rules, in order:
///  1. Only the LISTENING socket owner is considered. A bare `lsof -ti :PORT`
///     also matches *client* connections, and this app keeps pooled keep-alive
///     connections to the func management APIs — so the unfiltered form can
///     SIGKILL ais-runner itself.
///  2. Our own pid is never killed, belt-and-braces: ais-runner never listens
///     on these ports, so a self match can only be a lookup bug.
///
/// Returns `None` when nothing is listening (nothing to do).
pub fn kill_listener(port: u16) -> Option<u32> {
    let owner = owner(port)?;
    if owner.pid == std::process::id() {
        return None;
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &owner.pid.to_string()])
            .output();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &owner.pid.to_string()])
            .status();
    }
    Some(owner.pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_listener_never_targets_our_own_pid() {
        // Bind a port in-process so *we* are the listener, then assert
        // kill_listener refuses to act on it (it would kill this test run).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(kill_listener(port), None, "must never kill our own pid");
        // Still alive and still bound — proof we did not shoot ourselves.
        assert!(listener.local_addr().is_ok());
    }

    #[test]
    fn belongs_to_matches_same_project_only() {
        let o = PortOwner {
            pid: 123,
            detail:
                "/Users/x/code/projects/alpha_platform/function_apps/target/azure-functions/ais-functions"
                    .into(),
        };
        assert!(belongs_to(&o, "/Users/x/code/projects/alpha_platform"));
        assert!(belongs_to(&o, "/Users/x/code/projects/alpha_platform/")); // trailing slash tolerated
                                                                           // A different clone must NOT match.
        assert!(!belongs_to(
            &o,
            "/Users/x/code/projects/alpha_extra_platform"
        ));
        // Empty / unknown detail never matches.
        let unknown = PortOwner {
            pid: 1,
            detail: String::new(),
        };
        assert!(!belongs_to(
            &unknown,
            "/Users/x/code/projects/alpha_extra_platform"
        ));
    }
}
