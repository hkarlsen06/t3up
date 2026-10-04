//! The real binary in a pseudo-terminal against a fake ssh: it starts, draws, hands the terminal to an
//! ssh session while a check keeps running in the background, comes back, and quits leaving the
//! terminal as it found it.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const ALT_ON: &[u8] = b"\x1b[?1049h";
const ALT_OFF: &[u8] = b"\x1b[?1049l";

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// The master side of the pty: what the program printed, and a way to type at it.
struct Term {
    rx: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    out: Vec<u8>,
}

impl Term {
    fn send(&mut self, keys: &[u8]) {
        self.writer.write_all(keys).unwrap();
        self.writer.flush().unwrap();
    }

    /// Read until `until` appears after `from`, answering the image probe as a terminal does.
    fn pump(&mut self, from: usize, until: &[u8], secs: u64) -> usize {
        let end = Instant::now() + Duration::from_secs(secs);
        let mut answered = from;
        loop {
            if let Some(at) = find(&self.out, until, from) {
                return at + until.len();
            }
            let seen = String::from_utf8_lossy(&self.out[from.min(self.out.len())..]).into_owned();
            assert!(Instant::now() < end, "timed out waiting for {:?}; got {seen:?}", String::from_utf8_lossy(until));
            if let Ok(chunk) = self.rx.recv_timeout(Duration::from_millis(50)) {
                self.out.extend(chunk);
            }
            // The picker's probe ends with a device status report; every real terminal answers it.
            // Without that, it would keep reading the keyboard.
            while let Some(at) = find(&self.out, b"\x1b[5n", answered) {
                self.writer.write_all(b"\x1b[?62;22c\x1b[0n").unwrap();
                answered = at + 4;
            }
        }
    }
}

#[test]
fn session_in_a_real_terminal() {
    let dir = std::env::temp_dir().join(format!("t3up-pty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("servers"), "# test\n\nfirst # unavailable\nsecond\n").unwrap();
    let ssh = dir.join("ssh");
    std::fs::write(
        &ssh,
        r#"#!/bin/sh
while [ "$1" = -o ]; do shift 2; done
printf '%s %s\n' "$1" "$2" >> "$CALLS"
if [ -z "$2" ]; then sleep 3.5; cat "$XDG_STATE_HOME"/t3up/*/second-*-check.log > "$CALLS.saw"; exit 0; fi
cat > /dev/null
sleep 2
if [ "$1" = first ]; then echo 'Connection refused' >&2; exit 255; fi
printf '@@t3up\tbegin\tCodex\n@@t3up\tdone\tCodex: codex-cli 1.0.0 -> codex-cli 1.1.0\n'
printf '@@t3up\tversion\t1.0.0\n@@t3up\tdone\tHealth OK\n@@t3up\tcomplete\tOK\n'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let pair = native_pty_system().openpty(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 }).unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_t3up"));
    for (k, v) in [
        ("T3UP_SSH", ssh.to_str().unwrap()),
        ("T3UP_SERVERS_FILE", dir.join("servers").to_str().unwrap()),
        ("XDG_STATE_HOME", dir.join("state").to_str().unwrap()),
        ("CALLS", dir.join("calls").to_str().unwrap()),
        ("T3UP_REGISTRY", "file:///nonexistent"),
        ("T3UP_RELEASES_API", "off"),
        ("HOME", dir.to_str().unwrap()),
        ("TERM", "xterm-256color"),
    ] {
        cmd.env(k, v);
    }
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut term = Term { rx, writer, out: vec![] };
    let alt = term.pump(0, ALT_ON, 30);
    // The dashboard draws both servers.
    let drawn = term.pump(alt, b"second", 30);
    term.pump(drawn, b"first", 5);
    // Press `s` while the check is still running: the terminal goes to ssh, jobs keep going.
    term.send(b"s");
    let left = term.pump(drawn, ALT_OFF, 10);
    let back = term.pump(left, ALT_ON, 15);
    // Back on the dashboard; the check finished meanwhile, so q quits at once.
    term.pump(back, b"second", 5);
    std::thread::sleep(Duration::from_millis(300));
    term.send(b"q");
    let end = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < end, "t3up did not quit");
        if let Ok(chunk) = term.rx.recv_timeout(Duration::from_millis(50)) {
            term.out.extend(chunk);
        }
    };
    while let Ok(chunk) = term.rx.recv_timeout(Duration::from_millis(200)) {
        term.out.extend(chunk);
    }
    let out = term.out;
    assert!(status.success(), "exit {status:?}");
    let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
    assert!(calls.lines().any(|l| l == "first "), "no ssh session: {calls:?}");
    assert!(std::fs::read_to_string(dir.join("calls.saw")).unwrap().contains("complete"), "jobs stalled during ssh");
    // The terminal is left as it was found: normal screen, cursor back, mouse and paste modes off.
    let tail = &out[out.len().saturating_sub(300)..];
    for restore in [ALT_OFF, b"\x1b[?25h", b"\x1b[?1000l", b"\x1b[?2004l"] {
        assert!(
            find(tail, restore, 0).is_some(),
            "{:?} missing from the end: {:?}",
            String::from_utf8_lossy(restore),
            String::from_utf8_lossy(tail)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn termination_signals_end_an_ssh_handoff() {
    let mut results = vec![];
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        let dir = std::env::temp_dir().join(format!("t3up-pty-signal-{}-{signal}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("servers"), "host\n").unwrap();
        let ssh = dir.join("ssh");
        std::fs::write(
            &ssh,
            r#"#!/bin/sh
while [ "$1" = -o ]; do shift 2; done
if [ -z "$2" ]; then echo $$ > "$HOME/session-pid"
else cat >/dev/null; echo $$ > "$HOME/job-pid"; fi
exec /bin/sleep 30
"#,
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pair =
            native_pty_system().openpty(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 }).unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_t3up"));
        for (k, v) in [
            ("T3UP_SSH", ssh.to_str().unwrap()),
            ("T3UP_SERVERS_FILE", dir.join("servers").to_str().unwrap()),
            ("XDG_STATE_HOME", dir.join("state").to_str().unwrap()),
            ("T3UP_REGISTRY", "file:///nonexistent"),
            ("T3UP_RELEASES_API", "off"),
            ("HOME", dir.to_str().unwrap()),
            ("TERM", "xterm-256color"),
            ("T3UP_NO_IMAGES", "1"),
            ("T3UP_NO_MOTION", "1"),
        ] {
            cmd.env(k, v);
        }
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let pid = child.process_id().unwrap() as i32;
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut term = Term { rx, writer, out: vec![] };
        let drawn = term.pump(0, b"host", 10);
        term.send(b"s");
        term.pump(drawn, ALT_OFF, 5);
        let end = Instant::now() + Duration::from_secs(3);
        while !dir.join("session-pid").exists() || !dir.join("job-pid").exists() {
            assert!(Instant::now() < end, "fake SSH did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        let ssh_pids: Vec<i32> = ["session-pid", "job-pid"]
            .iter()
            .map(|f| std::fs::read_to_string(dir.join(f)).unwrap().trim().parse().unwrap())
            .collect();
        // SAFETY: signal only the t3up child spawned by this test.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
        let end = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= end {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let end = Instant::now() + Duration::from_secs(2);
        // SAFETY: signal 0 checks only this test's fake SSH processes.
        while ssh_pids.iter().any(|&pid| unsafe { libc::kill(pid, 0) } == 0) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        let reaped = ssh_pids.iter().all(|&pid| unsafe { libc::kill(pid, 0) } != 0);
        // Clean up even when the regression is present, before reporting the failed assertion.
        if status.is_none() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        for pid in ssh_pids {
            // SAFETY: kill only fake SSH processes started by this test, if still alive.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&dir);
        results.push((signal, status.is_some_and(|s| s.success()), reaped));
    }
    for (signal, exited, reaped) in results {
        assert!(exited, "signal {signal} did not shut down t3up during SSH");
        assert!(reaped, "signal {signal} left fake SSH running");
    }
}
