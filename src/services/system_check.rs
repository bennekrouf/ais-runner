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
    ]
}

fn probe(name: &'static str, args: &[&str], install_hint: &'static str) -> ToolStatus {
    match Command::new(name).args(args).output() {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).to_string();
            // first non-empty line, trimmed
            let version = raw.lines()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .map(|l| l.to_string());
            ToolStatus { name, available: true, version, install_hint }
        }
        _ => ToolStatus { name, available: false, version: None, install_hint },
    }
}
