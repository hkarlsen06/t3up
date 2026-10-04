//! One remote run: SSH output parsed into `model::Event`s.
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};

use regex::Regex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;

use crate::model::{self, Event, Mode};

/// Events from every running job, tagged with the host they came from.
pub type Sender = UnboundedSender<(String, Event)>;

#[derive(Debug, Clone)]
pub struct Job {
    pub host: String,
    pub mode: Mode,
    /// Exact T3 version to install; '' for the latest nightly.
    pub version: String,
    /// Comma-separated components to update, or 'all'.
    pub only: String,
    /// This run's log directory; the job writes `<host>-<HHMMSS>-<mode>.log` in it.
    pub logs: PathBuf,
    /// The script piped to the server: `model::remote_script()` (tests pass a variant).
    pub script: Arc<str>,
}

pub fn ssh_program() -> String {
    std::env::var("T3UP_SSH").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "ssh".into())
}

static TAG: LazyLock<Regex> = LazyLock::new(|| {
    let names = model::COMPONENTS.iter().map(|(n, _)| regex::escape(n)).collect::<Vec<_>>().join("|");
    Regex::new(&format!("^({names})\\| ?(.*)")).unwrap()
});

fn parse(raw: &[u8]) -> Option<Event> {
    let text = model::clean(&String::from_utf8_lossy(raw));
    let tagged = TAG.captures(&text);
    let (tag, text) =
        tagged.as_ref().map_or(("", text.as_str()), |c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()));
    if let Some(rest) = text.strip_prefix("@@t3up\t") {
        let (kind, detail) = rest.split_once('\t').unwrap_or((rest, ""));
        let detail = detail.to_string();
        Some(match kind {
            "begin" => Event::Begin(detail),
            "done" => Event::Done(detail),
            "skip" => Event::Skip(detail),
            "fail" => Event::Fail(detail),
            "auth" => Event::Auth(detail),
            "version" => Event::Version(detail),
            "busy" => Event::Busy(detail.parse().ok()?),
            "sys" => Event::Sys(detail),
            "rollback" => Event::Rollback(detail),
            "complete" => Event::Complete,
            _ => return None,
        })
    } else {
        (!text.is_empty()).then(|| Event::Output { tag: tag.into(), text: text.into() })
    }
}

/// Split on CR or LF. Cut oversized lines without making noisy installers fatal.
struct Lines<R> {
    stream: R,
    buffer: Vec<u8>,
    ready: VecDeque<Vec<u8>>,
    eof: bool,
}

impl<R: AsyncRead + Unpin> Lines<R> {
    fn new(stream: R) -> Self {
        Self { stream, buffer: vec![], ready: VecDeque::new(), eof: false }
    }

    async fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        const LIMIT: usize = 1 << 16;
        loop {
            if let Some(line) = self.ready.pop_front() {
                return Ok(Some(line));
            }
            if self.eof {
                return Ok((!self.buffer.is_empty()).then(|| std::mem::take(&mut self.buffer)));
            }
            let mut chunk = vec![0; LIMIT];
            let n = self.stream.read(&mut chunk).await?;
            self.eof = n == 0;
            self.buffer.extend_from_slice(&chunk[..n]);
            let mut start = 0;
            for (i, &byte) in self.buffer.iter().enumerate() {
                if byte == b'\r' || byte == b'\n' {
                    self.ready.push_back(self.buffer[start..i].to_vec());
                    start = i + 1;
                }
            }
            self.buffer.drain(..start);
            if self.buffer.len() > LIMIT {
                self.ready.push_back(std::mem::take(&mut self.buffer));
            }
        }
    }
}

/// A single kernel pipe preserves the order of SSH's stdout and stderr writes.
fn output_pipe() -> io::Result<(tokio::process::ChildStdout, Stdio, Stdio)> {
    let (read, write) = io::pipe()?;
    let stderr = write.try_clone()?;
    #[cfg(unix)]
    let read: std::os::fd::OwnedFd = read.into();
    #[cfg(windows)]
    let read: std::os::windows::io::OwnedHandle = read.into();
    Ok((tokio::process::ChildStdout::from_std(read.into())?, write.into(), stderr.into()))
}

/// Sends Start before SSH starts and Exit last. Dropping this future kills SSH.
pub async fn run_job(job: Job, tx: Sender) {
    let log =
        job.logs.join(format!("{}-{}-{}.log", job.host, chrono::Local::now().format("%H%M%S"), job.mode.as_str()));
    let send = |event| {
        let _ = tx.send((job.host.clone(), event));
    };
    send(Event::Start { log: log.clone() });
    let result: io::Result<Option<i32>> = async {
        tokio::fs::create_dir_all(&job.logs).await?;
        let mut log = tokio::fs::File::create(log).await?;
        let (read, stdout, stderr) = output_pipe()?;
        let remote = shlex::try_join(["sh", "-s", "--", job.mode.as_str(), &job.version, &job.only])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut command = tokio::process::Command::new(ssh_program());
        command
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                &job.host,
                &remote,
            ])
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        // Command retains the pipe's write descriptors; release them so EOF can arrive.
        drop(command);
        let mut stdin = child.stdin.take().unwrap();
        let write = async {
            let result = stdin.write_all(job.script.as_bytes()).await;
            drop(stdin);
            match result {
                Err(e) if matches!(e.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset) => Ok(()),
                result => result,
            }
        };
        let read = async {
            let mut lines = Lines::new(read);
            while let Some(raw) = lines.next().await? {
                log.write_all(&raw).await?;
                log.write_all(b"\n").await?;
                log.flush().await?;
                if let Some(event) = parse(&raw) {
                    send(event);
                }
            }
            Ok::<_, io::Error>(())
        };
        tokio::try_join!(write, read)?;
        Ok(child.wait().await?.code())
    }
    .await;
    let (code, error) = match result {
        Ok(code) => (code, None),
        Err(e) => (None, Some(e.to_string())),
    };
    send(Event::Exit { code, error });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn overlong_lines() {
        let mut input = vec![b'x'; 300_000];
        input.extend_from_slice(b"\rprogress\r\nok\n");
        let mut lines = Lines::new(input.as_slice());
        let mut sizes = vec![];
        while let Some(line) = lines.next().await.unwrap() {
            sizes.push(line.len());
        }
        assert_eq!(&sizes[sizes.len() - 3..], &[8, 0, 2]);
        assert_eq!(sizes[..sizes.len() - 3].iter().sum::<usize>(), 300_000);
        assert!(sizes.iter().all(|&n| n <= 2 << 16));
    }

    #[test]
    fn events() {
        assert_eq!(
            parse(b"\x1b[32mCodex| @@t3up\tdone\tCodex: 1.0.0\x1b[0m"),
            Some(Event::Done("Codex: 1.0.0".into()))
        );
        assert_eq!(parse(b"Pi| progress"), Some(Event::Output { tag: "Pi".into(), text: "progress".into() }));
        assert_eq!(parse(b"@@t3up\tbusy\t2"), Some(Event::Busy(2)));
        assert_eq!(parse(b"@@t3up\tunknown\tanything"), None);
    }
}
