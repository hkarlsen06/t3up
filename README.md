<p align="center"><img src="icon.svg" width="96" alt="t3up icon"></p>

<h1 align="center">t3up</h1>

<p align="center">
A full-screen terminal dashboard for checking and updating your <a href="https://github.com/pingdotgg/t3code">T3 Code</a> servers over SSH.
</p>

<p align="center">
<a href="https://github.com/hkarlsen06/t3up/actions/workflows/test.yml"><img src="https://github.com/hkarlsen06/t3up/actions/workflows/test.yml/badge.svg" alt="test"></a>
<a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT license"></a>
</p>

![t3up dashboard](docs/screenshot.png)

t3up shows every server you run T3 Code on, with the version of T3 and each provider CLI it drives
(Codex, Claude Code, OpenCode, Grok, Pi), whether each one is signed in, and whether a newer release is out.
You can update one server or all of them in parallel without leaving the terminal.

- **Check is read-only.** The dashboard checks every server on launch, and checking never changes anything.
- **Parallel.** All servers, and all components on each server, update at the same time.
- **Updates tools the way they were installed.** A provider is updated with its own updater if it installed itself,
  otherwise with whichever of npm, pnpm, bun or Homebrew owns it. If t3up can't tell, it fails and says so instead of guessing.
- **Sign-in built in.** When an update leaves a tool signed out, t3up opens its login on that server for you.
- **No agent on the server.** It runs a POSIX `sh` script over plain SSH. Nothing to install remotely.
- **Scriptable.** `--check` and `--update` give plain output and an exit code, for cron or CI.
- **Desktop app (macOS).** When your T3 Code desktop app is older than your servers, t3up can update it as well.

## Install

All you need is [uv](https://docs.astral.sh/uv/getting-started/installation/). t3up is a single file, and uv
fetches Python 3.12+ and the dependencies the first time you run it.

**macOS and Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/hkarlsen06/t3up/main/t3up -o ~/.local/bin/t3up
chmod +x ~/.local/bin/t3up
t3up
```

**Windows** (PowerShell, using the built-in OpenSSH client)

```powershell
irm https://raw.githubusercontent.com/hkarlsen06/t3up/main/t3up -OutFile t3up
uv run --script t3up
```

You can also try it without installing: `uv run --script https://raw.githubusercontent.com/hkarlsen06/t3up/main/t3up`.

## Set up servers

Each server needs key-based SSH access (t3up runs `ssh` in batch mode, so it never asks for a password) and T3 Code installed.

Press <kbd>e</kbd> in the dashboard to add servers. It suggests hosts from your `~/.ssh/config`.
Or edit `~/.config/t3up/servers` yourself, with one SSH alias or `user@host` per line:

```
# t3up servers
build-01
deploy@203.0.113.10
```

## Use

Run `t3up` with no arguments to open the dashboard.

| Key | Action |
| --- | --- |
| <kbd>Enter</kbd> / click | Actions for the selected server: update, check again, show output, open a terminal |
| <kbd>a</kbd> | Update all servers |
| <kbd>r</kbd> | Check every server again |
| <kbd>v</kbd> | Pin the T3 version updates install (blank means the latest nightly) |
| <kbd>e</kbd> | Add or remove servers |
| <kbd>s</kbd> | Open an SSH session on the selected server |
| <kbd>l</kbd> | Show or hide the live output |
| <kbd>d</kbd> | Update the T3 Code desktop app on this machine (macOS) |
| <kbd>Ctrl</kbd>+<kbd>P</kbd> | Command palette |
| <kbd>q</kbd> | Quit |

"Everything" updates T3 and every provider that's already installed. A missing provider is installed
(per user, without sudo) only if you pick it by name.

### Headless

```sh
t3up --check                       # versions and health of every server, changes nothing
t3up --update                      # update T3 and every installed provider everywhere
t3up --only codex,claude           # update just these (installs them where missing)
t3up --update --host build-01      # just one server (repeatable)
t3up 0.0.46-nightly.20261003.2632  # install this exact T3 version
t3up --desktop                     # update the macOS desktop app, then exit
```

The exit code is non-zero if any server failed. Every run writes per-server logs to `~/.local/state/t3up/`.

## How it works

For each server, t3up runs `ssh HOST sh -s` and sends a small POSIX shell script over stdin. The script checks or
updates each component in parallel, then confirms the T3 server answers on `localhost:3773`. It reports progress as
tagged lines, which the dashboard reads. A server only counts as updated if SSH exits cleanly **and** the script
reports that it finished.

Servers can run any POSIX `sh` (it's tested under `dash`). The newest release of each tool is looked up on the
npm registry.

## Development

```sh
./test_t3up.py
```

The test puts a fake `ssh` on `PATH` and never contacts a real server. It covers headless mode, the dashboard
(through Textual's pilot), and the remote script running under `dash`. It needs a Unix-like system with `dash` installed.

## License

[MIT](LICENSE)
