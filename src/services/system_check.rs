use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolStatus {
    pub name:         &'static str,
    pub available:    bool,
    pub version:      Option<String>,
    pub install_hint: &'static str,
}

pub fn check_tools() -> Vec<ToolStatus> {
    vec![
        probe("func",    &["--version"], "npm install -g azure-functions-core-tools@4"),
        probe("azurite", &["--version"], "npm install -g azurite"),
        probe("az",      &["--version"], "https://aka.ms/installazurecli"),
        probe("node",    &["--version"], "https://nodejs.org"),
        probe("mvn",     &["--version"], "https://maven.apache.org/install.html"),
        probe_docker(),
    ]
}

/// Check Docker: distinguish between not installed, installed-but-stopped,
/// and running.  The install_hint guides the user accordingly.
fn probe_docker() -> ToolStatus {
    // Step 1 — is the docker CLI present?
    let cli = Command::new("docker").args(["--version"]).output();
    let version = match &cli {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).to_string();
            raw.lines()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .map(|l| l.to_string())
        }
        _ => None,
    };

    if version.is_none() {
        // Docker CLI not found at all
        return ToolStatus {
            name:         "docker",
            available:    false,
            version:      None,
            install_hint: "https://www.docker.com/products/docker-desktop/",
        };
    }

    // Step 2 — is the daemon running?  `docker info` exits non-zero when stopped.
    let running = Command::new("docker")
        .args(["info"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if running {
        ToolStatus {
            name:         "docker",
            available:    true,
            version,
            install_hint: "",
        }
    } else {
        // Installed but daemon not running — mark unavailable with a clear hint
        ToolStatus {
            name:         "docker",
            available:    false,
            version,   // version present = installed, just not running
            install_hint: "Docker Desktop is installed but not running — start it from the Start Menu",
        }
    }
}

fn probe(name: &'static str, args: &[&str], install_hint: &'static str) -> ToolStatus {
    match Command::new(name).args(args).output() {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).to_string();
            let version = raw.lines()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .map(|l| l.to_string());
            ToolStatus { name, available: true, version, install_hint }
        }
        _ => ToolStatus { name, available: false, version: None, install_hint },
    }
}
