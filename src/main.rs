use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, error::ErrorKind};
use t3up::{config, desktop, headless, model, tui};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Interactive dashboard for your T3 coding servers. Run without options to open it; use --check/--update for scripts.",
    after_help = "Servers live in ~/.config/t3up/servers, one SSH alias or user@host per line. Press e in the dashboard to edit them."
)]
struct Cli {
    /// Headless update to this exact T3 version
    #[arg(value_name = "VERSION")]
    target: Option<String>,
    /// Headless: check versions and health, change nothing
    #[arg(long)]
    check: bool,
    /// Headless: update servers
    #[arg(long)]
    update: bool,
    /// Headless update: only these, comma-separated (t3,codex,claude,opencode,grok,pi); installs a
    /// missing provider. Default all: T3 and every installed provider
    #[arg(long, value_name = "NAMES", value_parser = model::parse_only)]
    only: Option<String>,
    /// Only this configured host (repeatable)
    #[arg(long, value_name = "HOST")]
    host: Vec<String>,
    /// Headless update: update every server at once, instead of the first one alone first
    #[arg(long)]
    all_at_once: bool,
    /// List configured hosts and exit
    #[arg(long)]
    list: bool,
    /// Headless: show installer output
    #[arg(long, short)]
    verbose: bool,
    /// Update the T3 Code desktop app on this machine (quits and reopens it), then exit
    #[arg(long)]
    desktop: bool,
}

fn usage(kind: ErrorKind, message: impl std::fmt::Display) -> ! {
    Cli::command().error(kind, message).exit()
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.desktop {
        return match desktop::update_desktop(&|text| println!("{text}")) {
            Ok(version) => {
                println!("T3 Code desktop {version}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Desktop update failed: {error}");
                ExitCode::from(1)
            }
        };
    }
    let version = cli.target.clone().unwrap_or_default();
    if !version.is_empty() && !model::VERSION_RE.is_match(&version) {
        usage(ErrorKind::InvalidValue, "VERSION must be an exact version, for example 0.0.46-nightly.20261003.2623");
    }
    if cli.check && (!version.is_empty() || cli.update || cli.only.is_some()) {
        usage(ErrorKind::ArgumentConflict, "--check does not update");
    }
    let (_, mut hosts) = config::read_servers().unwrap_or_else(|e| usage(ErrorKind::Io, e));
    let headless_run = cli.check || cli.update || !version.is_empty() || cli.only.is_some();
    if hosts.is_empty() && (headless_run || !cli.host.is_empty()) {
        usage(
            ErrorKind::InvalidValue,
            format!("No servers in {}; run t3up and press e to add some", config::servers_file().display()),
        );
    }
    if !cli.host.is_empty() {
        if let Some(unknown) = cli.host.iter().find(|h| !hosts.contains(h)) {
            usage(
                ErrorKind::InvalidValue,
                format!("Unknown host {unknown}; add it to {}", config::servers_file().display()),
            );
        }
        hosts.retain(|h| cli.host.contains(h));
    }
    if cli.list {
        for host in &hosts {
            println!("{host}");
        }
        return ExitCode::SUCCESS;
    }
    config::private_umask();
    let logs = config::run_logs();
    if headless_run {
        let opts = headless::Options {
            hosts,
            mode: if cli.check { model::Mode::Check } else { model::Mode::Update },
            version,
            only: cli.only.unwrap_or_else(|| "all".into()),
            verbose: cli.verbose,
            canary: !cli.all_at_once,
        };
        let desktop = tokio::task::spawn_blocking(desktop::desktop_version).await.unwrap_or_default();
        return ExitCode::from(headless::run(opts, desktop, logs).await as u8);
    }
    if !std::io::stdout().is_terminal() {
        usage(ErrorKind::MissingRequiredArgument, "no terminal; use --check or --update");
    }
    match tui::run(hosts, logs).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("t3up: {error:#}");
            ExitCode::from(1)
        }
    }
}
