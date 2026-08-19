//! Headless scenario runner.
//!
//! Runs the scenarios saved under `<project>/.ais-runner/scenarios` without the
//! GUI, so a suite authored in the Tests view can also run from a terminal or a
//! CI agent. The emulators and `func start` must already be up — this binary
//! drives scenarios, it does not provision the environment.
//!
//! Exit codes: 0 = every step passed, 1 = at least one step failed,
//! 2 = usage or setup error (bad arguments, no scenarios, unreadable files).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ais_runner::services::ci_export;
use ais_runner::services::cosmos_check;
use ais_runner::services::junit::{self, SuiteReport};
use ais_runner::services::scenario::{self, RunContext, StepStatus};

const USAGE: &str = "\
ais-test — run ais-runner scenarios headlessly

USAGE:
    ais-test <project-dir> [OPTIONS]

ARGS:
    <project-dir>    Project root containing .ais-runner/scenarios

OPTIONS:
    --scenario <name>       Run only scenarios whose name contains <name>.
                            Repeatable; matching is case-insensitive.
    --junit <file>          Write a JUnit XML report to <file>.
    --list                  List discovered scenarios and exit.
    --emit-ci <dir>         Generate docker-compose.test.yml + the Service Bus
                            emulator's Config.json into <dir>, then exit.
                            Regenerate whenever workflows change queues.
    --sb-host <host>        Service Bus host           [default: 127.0.0.1]
    --cosmos-endpoint <url> Cosmos endpoint            [default: emulator]
    --cosmos-key <key>      Cosmos master key          [default: emulator]
    -h, --help              Show this help

EXIT CODES:
    0  all steps passed
    1  at least one step failed
    2  usage or setup error
";

struct Args {
    project_dir: PathBuf,
    filters: Vec<String>,
    junit_path: Option<PathBuf>,
    list_only: bool,
    emit_ci: Option<PathBuf>,
    sb_host: String,
    cosmos_endpoint: String,
    cosmos_key: String,
}

fn parse_args() -> Result<Args, String> {
    let mut raw = std::env::args().skip(1);
    let mut project_dir: Option<PathBuf> = None;
    let mut args = Args {
        project_dir: PathBuf::new(),
        filters: Vec::new(),
        junit_path: None,
        list_only: false,
        emit_ci: None,
        sb_host: "127.0.0.1".to_string(),
        cosmos_endpoint: cosmos_check::EMULATOR_ENDPOINT.to_string(),
        cosmos_key: cosmos_check::EMULATOR_KEY.to_string(),
    };

    // A tiny hand-rolled parser rather than a CLI crate: this is the only
    // command in the binary and adding a dependency for six flags isn't worth it.
    while let Some(arg) = raw.next() {
        let mut value = |name: &str| -> Result<String, String> {
            raw.next().ok_or_else(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--list" => args.list_only = true,
            "--scenario" => args.filters.push(value("--scenario")?.to_lowercase()),
            "--junit" => args.junit_path = Some(PathBuf::from(value("--junit")?)),
            "--emit-ci" => args.emit_ci = Some(PathBuf::from(value("--emit-ci")?)),
            "--sb-host" => args.sb_host = value("--sb-host")?,
            "--cosmos-endpoint" => args.cosmos_endpoint = value("--cosmos-endpoint")?,
            "--cosmos-key" => args.cosmos_key = value("--cosmos-key")?,
            other if other.starts_with('-') => {
                return Err(format!("unknown option '{other}'"));
            }
            other => {
                if project_dir.is_some() {
                    return Err(format!("unexpected argument '{other}'"));
                }
                project_dir = Some(PathBuf::from(other));
            }
        }
    }

    args.project_dir = project_dir.ok_or("missing <project-dir>")?;
    Ok(args)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let root = match args.project_dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", args.project_dir.display());
            return ExitCode::from(2);
        }
    };

    // Generating the harness only reads the workflows — it needs no scenarios,
    // so handle it before discovery and its "nothing found" error.
    if let Some(out) = &args.emit_ci {
        return emit_ci(&root, out);
    }

    let (all, errors) = scenario::discover(&root);
    for err in &errors {
        eprintln!("warning: {err}");
    }

    let selected: Vec<_> = all
        .into_iter()
        .filter(|s| {
            args.filters.is_empty()
                || args.filters.iter().any(|f| s.name.to_lowercase().contains(f))
        })
        .collect();

    if args.list_only {
        for s in &selected {
            println!("{}  ({} step(s))", s.name, s.steps.len());
        }
        return ExitCode::SUCCESS;
    }

    if selected.is_empty() {
        eprintln!(
            "error: no scenarios found in {}",
            root.join(".ais-runner/scenarios").display()
        );
        // A discovery error alongside an empty list almost always means a
        // malformed file rather than an empty directory — say so, since the
        // two look identical from the exit code alone.
        if !errors.is_empty() {
            eprintln!("       ({} file(s) failed to parse — see warnings above)", errors.len());
        }
        return ExitCode::from(2);
    }

    let ctx = RunContext {
        sb_host: args.sb_host.clone(),
        cosmos_endpoint: args.cosmos_endpoint.clone(),
        cosmos_key: args.cosmos_key.clone(),
        project_root: root.clone(),
        // Restarting the Functions host is the GUI's job; in CI the host is
        // managed by the pipeline. A `restart_func` step reports this rather
        // than silently doing nothing.
        restart_func: None,
    };

    let mut suites: Vec<SuiteReport> = Vec::new();

    for s in &selected {
        println!("\n▶ {} ({} step(s))", s.name, s.steps.len());

        let steps = scenario::run(s, &ctx, |r| {
            let mark = match r.status {
                StepStatus::Ok => "ok  ",
                StepStatus::Failed => "FAIL",
                StepStatus::Skipped => "skip",
            };
            println!(
                "  [{mark}] {:02}. {}  ({}ms)",
                r.index + 1,
                r.label,
                r.elapsed_ms
            );
            // Only failures get their detail echoed: on a passing run the
            // details are noise, and on a failing one this is the message the
            // user actually needs.
            if r.status == StepStatus::Failed {
                println!("         {}", r.detail);
            }
        })
        .await;

        suites.push(SuiteReport {
            scenario: s.name.clone(),
            steps,
        });
    }

    print_summary(&suites);

    if let Some(path) = &args.junit_path {
        if let Err(e) = write_junit(path, &suites) {
            eprintln!("error: could not write {}: {e}", path.display());
            return ExitCode::from(2);
        }
        println!("JUnit report: {}", path.display());
    }

    if suites.iter().all(|s| s.passed()) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Write the compose file and Service Bus config for a CI run.
fn emit_ci(root: &Path, out: &Path) -> ExitCode {
    let generated = match ci_export::generate(&root.to_string_lossy(), out) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    for file in &generated.files {
        println!("wrote {}", file.display());
    }
    println!("{} queue(s) declared", generated.queues.len());
    for note in &generated.notes {
        eprintln!("warning: {note}");
    }
    if !generated.dropped.is_empty() {
        eprintln!(
            "warning: {} queue(s) dropped over the emulator's entity ceiling: {}",
            generated.dropped.len(),
            generated.dropped.join(", ")
        );
    }
    ExitCode::SUCCESS
}

fn print_summary(suites: &[SuiteReport]) {
    let total: usize = suites.iter().map(|s| s.steps.len()).sum();
    let failures: usize = suites.iter().map(|s| s.failures()).sum();
    let skipped: usize = suites.iter().map(|s| s.skipped()).sum();
    let failed_suites: Vec<&SuiteReport> = suites.iter().filter(|s| !s.passed()).collect();

    println!("\n─────────────────────────────────────────────");
    println!(
        "{} scenario(s), {total} step(s): {} passed, {failures} failed, {skipped} skipped",
        suites.len(),
        total - failures - skipped,
    );

    // Repeat the failures at the end: on a long run the first failure has
    // scrolled far out of view by the time the suite finishes.
    for suite in failed_suites {
        for step in suite.steps.iter().filter(|s| s.status == StepStatus::Failed) {
            println!("  FAIL  {} → {:02}. {}", suite.scenario, step.index + 1, step.label);
            println!("        {}", step.detail);
        }
    }
}

fn write_junit(path: &Path, suites: &[SuiteReport]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, junit::render(suites))
}
