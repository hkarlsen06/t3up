use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, error::ErrorKind};
use t3up::{config, desktop, headless, model, selfupdate, tui};

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
    /// Headless: remove these providers, comma-separated (codex,claude,opencode,grok,pi); their
    /// settings and sign-in stay
    #[arg(long, value_name = "NAMES", value_parser = model::parse_providers)]
    remove: Option<String>,
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
    /// Update t3up itself to the newest release, then exit
    #[arg(long)]
    self_update: bool,
}

fn usage(kind: ErrorKind, message: impl std::fmt::Display) -> ! {
    Cli::command().error(kind, message).exit()
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.self_update {
        let Some(version) = tokio::task::spawn_blocking(selfupdate::available).await.ok().flatten() else {
            println!("t3up {} is the newest (or GitHub couldn't be reached)", selfupdate::current());
            return ExitCode::SUCCESS;
        };
        let done = tokio::task::spawn_blocking(move || selfupdate::update(&version, &|text| println!("{text}"))).await;
        return match done.unwrap_or_else(|_| Err("internal error".into())) {
            Ok(version) => {
                println!("t3up {version} installed");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("t3up update failed: {error}");
                ExitCode::from(1)
            }
        };
    }
    if cli.desktop {
        let version = cli.target.clone().unwrap_or_default();
        if !version.is_empty() && !model::VERSION_RE.is_match(&version) {
            usage(ErrorKind::InvalidValue, "VERSION must be an exact version, for example 0.0.45");
        }
        return match desktop::update_desktop(&version, &|text| println!("{text}")) {
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
    if cli.remove.is_some() && (cli.check || cli.update || cli.only.is_some() || !version.is_empty()) {
        usage(ErrorKind::ArgumentConflict, "--remove goes alone: it neither checks nor updates");
    }
    let (_, mut hosts) = config::read_servers().unwrap_or_else(|e| usage(ErrorKind::Io, e));
    let headless_run = cli.check || cli.update || !version.is_empty() || cli.only.is_some() || cli.remove.is_some();
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
            mode: if cli.check {
                model::Mode::Check
            } else if cli.remove.is_some() {
                model::Mode::Remove
            } else {
                model::Mode::Update
            },
            version,
            only: cli.remove.or(cli.only).unwrap_or_else(|| "all".into()),
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
        Ok(true) => restart(),
        Ok(false) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("t3up: {error:#}");
            ExitCode::from(1)
        }
    }
}

/// Start t3up again, in this terminal with the same arguments: it just updated itself.
fn restart() -> ExitCode {
    let Ok(exe) = selfupdate::executable() else { return ExitCode::SUCCESS };
    let mut command = std::process::Command::new(exe);
    command.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec(); // only returns if it failed
        eprintln!("t3up: couldn't restart: {error}; run t3up again");
        ExitCode::from(1)
    }
    #[cfg(not(unix))]
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(0) as u8),
        Err(error) => {
            eprintln!("t3up: couldn't restart: {error}; run t3up again");
            ExitCode::from(1)
        }
    }
}
