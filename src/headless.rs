//! Plain output for scripts: `t3up --check`, `--update`, `--only`, VERSION.
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use crate::{
    config, job,
    model::{self, Event, Host, Mode, Status},
    registry,
};

#[derive(Debug, Clone)]
pub struct Options {
    pub hosts: Vec<String>,
    pub mode: Mode,
    pub version: String,
    pub only: String,
    pub verbose: bool,
    /// Update the first server alone first, and the rest only if it comes back healthy.
    pub canary: bool,
}

/// Run every host, print progress and results. Returns the exit code.
pub async fn run(opts: Options, desktop: String, logs: PathBuf) -> i32 {
    let width = opts.hosts.iter().map(|n| n.len()).max().unwrap_or(0);
    let mut hosts: Vec<_> = opts.hosts.iter().map(|n| Host::new(n)).collect();
    let latest = tokio::task::spawn_blocking(registry::fetch_latest);
    let mine = tokio::task::spawn_blocking(crate::selfupdate::available);
    let script: Arc<str> = model::remote_script().into();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let start = |h: &mut Host| {
        h.reset(opts.mode, &opts.only);
        tokio::spawn(job::run_job(
            job::Job {
                host: h.name.clone(),
                mode: opts.mode,
                version: opts.version.clone(),
                only: opts.only.clone(),
                logs: logs.clone(),
                script: script.clone(),
                local: false,
            },
            tx.clone(),
        ));
    };
    println!("\nT3UP · {} · {} servers in parallel · from {}\n", opts.mode.as_str(), hosts.len(), config::local_name());
    let canary = opts.mode == Mode::Update && opts.canary && hosts.len() > 1;
    if canary {
        println!("Canary: updating {} first", hosts[0].name);
        start(&mut hosts[0]);
    } else {
        for h in &mut hosts {
            start(h);
        }
    }
    let mut remaining = if canary { 1 } else { hosts.len() };
    while remaining > 0 {
        let Some((name, event)) = rx.recv().await else {
            break;
        };
        let h = hosts.iter_mut().find(|h| h.name == name).unwrap();
        let output = matches!(event, Event::Output { .. });
        let exit = matches!(event, Event::Exit { .. });
        if let Some(line) = h.apply(event)
            && (!output || opts.verbose)
        {
            println!("{name:<width$}  {line}");
        }
        if exit {
            remaining -= 1;
            if h.status == Status::Failed {
                println!(
                    "{name:<width$}  ✗ {}\n{name:<width$}  log: {}",
                    h.error,
                    h.log.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
                );
            }
            for tool in &h.auth {
                if let Some(command) = model::login_command(tool) {
                    println!(
                        "{name:<width$}  ! {tool} {}; run: ssh -t {name} {}",
                        model::sign_in_what(tool),
                        shlex::try_quote(&command).unwrap()
                    );
                }
            }
            if canary && name == hosts[0].name && hosts[0].status == Status::Ok {
                for h in &mut hosts[1..] {
                    start(h);
                }
                remaining += hosts.len() - 1;
            }
        }
        let _ = std::io::stdout().flush();
    }
    let latest = latest.await.unwrap_or_default();
    println!("\nRESULTS");
    for h in &hosts {
        if h.status == Status::Idle {
            println!("  – {:<16} SKIPPED  (canary failed)", h.name);
            continue;
        }
        if h.status == Status::Ok && model::no_t3(h) {
            println!("  ○ {:<16} NO T3    install it: t3up --only t3 --host {}", h.name, h.name);
            continue;
        }
        let ok = h.status == Status::Ok;
        let new = model::updates(h, &latest, &opts.version)
            .iter()
            .map(|(name, v)| format!("{name} {}", model::compact(v, &h.current[name])))
            .collect::<Vec<_>>()
            .join(", ");
        print!(
            "  {} {:<16} {:<7} {:<35} {:.1}s",
            if ok { "✓" } else { "✗" },
            h.name,
            if ok { "OK" } else { "FAILED" },
            h.version,
            h.elapsed.as_secs_f64()
        );
        if !new.is_empty() && ok {
            print!("   update available: {new}");
        }
        if let Some(n) = h.busy.filter(|&n| n > 0) {
            print!(" · {n} agents live");
        }
        println!();
    }
    let refs: Vec<_> = hosts.iter().collect();
    let behind = model::desktop_behind(&desktop, &refs);
    if !behind.is_empty() {
        println!("\n! T3 Code on this machine is {desktop}, older than the servers ({behind}): run t3up --desktop.");
    }
    if let Ok(Some(version)) = mine.await {
        println!("\n! t3up {version} is out: run t3up --self-update.");
    }
    let passed = hosts.iter().filter(|h| h.status == Status::Ok).count();
    println!("\n{passed}/{} passed · Logs: {}\n", hosts.len(), logs.display());
    i32::from(passed != hosts.len())
}
