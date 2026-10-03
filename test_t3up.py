#!/usr/bin/env -S uv run --with-requirements t3up python3
"""Run with ./test_t3up.py from the repo root (shares t3up's pinned dependencies);
never contacts a server (fake ssh on PATH)."""
import asyncio
import fcntl
import os
import pty
import select
import struct
import termios
from pathlib import Path
import runpy
import shutil
import subprocess
import tempfile
import time

with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    (root / "servers").write_text("# test\n\nfirst # unavailable\nsecond")
    ssh = root / "ssh"
    ssh.write_text('''#!/bin/sh
while [ "$1" = -o ]; do shift 2; done
printf '%s %s\\n' "$1" "$2" >> "$CALLS"
if [ -z "$2" ]; then sleep 3.5; cat "$XDG_STATE_HOME"/t3up/*/second-*-check.log > "$CALLS.saw"; exit 0; fi
cat > /dev/null
sleep ${DELAY:-1}
if [ "$1" = first ]; then echo 'Connection refused' >&2; exit 255; fi
printf '@@t3up\\tbegin\\tCodex\\n@@t3up\\tdone\\tCodex: codex-cli 1.0.0 -> codex-cli 1.1.0\\n'
printf '@@t3up\\tauth\\tCodex\\n@@t3up\\tversion\\t1.0.0\\n@@t3up\\tdone\\tHealth OK\\n@@t3up\\tcomplete\\tOK\\n'
''')
    ssh.chmod(0o755)
    os.environ.update(PATH=f"{root}:{os.environ['PATH']}", CALLS=str(root / "calls"),
                      T3UP_SERVERS_FILE=str(root / "servers"), XDG_STATE_HOME=str(root / "state"))
    command = str(Path(__file__).resolve().with_name("t3up"))

    # Headless: parallel, continues past failures, plain output.
    started = time.monotonic()
    result = subprocess.run([command, "--check"], capture_output=True, text=True)
    assert time.monotonic() - started < 1.8, "hosts did not run in parallel"
    assert result.returncode == 1, result
    assert sorted(l.split()[0] for l in (root / "calls").read_text().splitlines()) == ["first", "second"]
    assert "second           OK" in result.stdout and "1/2 passed" in result.stdout, result.stdout
    assert "\033" not in result.stdout
    assert "! Codex not signed in; run: ssh -t second 'PATH=" in result.stdout, result.stdout
    (root / "calls").unlink()
    result = subprocess.run([command, "bad'; touch /tmp/no"], capture_output=True)
    assert result.returncode == 2 and not (root / "calls").exists()
    assert subprocess.run([command, "--check", "--only", "t3"], capture_output=True).returncode == 2
    subprocess.run([command, "--only", "codex", "--host", "second"], capture_output=True)
    assert (root / "calls").read_text().split("--", 1)[1].split()[::3] == ["update", "codex"]
    (root / "calls").unlink()

    # Real terminal: SSH session suspends the UI but background checks keep streaming.
    shutil.rmtree(root / "state")             # only this run's logs may satisfy the check
    os.environ["DELAY"] = "2"                # check still running when SSH opens
    pid, fd = pty.fork()
    if pid == 0:
        os.execv(command, [command])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    def pump(seconds, send=b""):
        if send: os.write(fd, send)
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if select.select([fd], [], [], 0.05)[0]:
                try: os.read(fd, 65536)
                except OSError: return
    pump(0.3); pump(4.5, b"s"); pump(1.0, b"q")
    del os.environ["DELAY"]
    _, code = os.waitpid(pid, 0)
    assert code == 0, code
    assert "complete" in (root / "calls.saw").read_text(), "jobs stalled during SSH"
    (root / "calls").unlink()

    # Interactive: auto-checks on launch (never updates), then update via menu.
    m = runpy.run_path(command)
    async def overlong():
        reader = asyncio.StreamReader()
        reader.feed_data(b"x" * 300_000 + b"\rprogress\r\nok\n")
        reader.feed_eof()
        return [len(line) for line in [l async for l in m["read_lines"](reader)]]
    sizes = asyncio.run(overlong())
    assert sizes[-3:] == [8, 0, 2] and sum(sizes[:-3]) == 300_000 and max(sizes) <= 2 << 16, sizes
    assert m["version_text"]("Codex: codex-cli 0.160.0 -> codex-cli 0.161.0") == "0.160.0 → 0.161.0"
    assert m["version_text"]("Claude: 2.1.288 (Claude Code) -> 2.1.288 (Claude Code)") == "2.1.288"
    n = "T3: t3 0.0.46-nightly.20261003."
    assert m["version_text"](n + "2623 -> t3 0.0.47-nightly.20261004.2701") == "0.0.46-nightly → 0.0.47-nightly"
    assert m["version_text"](n + "2623 -> t3 0.0.46-nightly.20261003.2633") == "0.0.46-nightly (…23 → …33)"
    assert m["version_text"](n + "2623 -> t3 0.0.46-nightly.20261004.2624") == "0.0.46-nightly (…3 → …4)"
    assert m["version_text"](n + "2623") == "0.0.46-nightly"

    async def drive():
        app = m["T3Up"](["first", "second"], root / "logs")
        terminals = []
        async def terminal(*command, banner=""):
            terminals.append(command)
        app.terminal = terminal
        async with app.run_test(size=(100, 30)) as pilot:
            await app.workers.wait_for_complete()
            await pilot.pause()
            assert [h.status for h in app.hosts] == ["failed", "ok"]
            assert all("check" in l for l in (root / "calls").read_text().splitlines())
            assert app.grid.styles.grid_size_columns == 2
            await pilot.press("down", "u")           # select "second", open update menu
            await pilot.press("down", "enter")       # "T3 server only"
            await pilot.pause()
            assert app.hosts[1].status == "running" and app.hosts[0].status == "failed"
            await app.workers.wait_for_complete()
            assert app.hosts[1].mode == "update"
            assert terminals == [], "check, or an update without Codex, must not sign in to Codex"
            await pilot.press("u", "down", "down", "enter")  # update Codex only: signed out -> sign in
            await app.workers.wait_for_complete()
            assert terminals and terminals[0][:3] == ("ssh", "-t", "second"), terminals
            assert terminals[0][3].endswith("codex login --device-auth"), terminals
            await app.workers.wait_for_complete()  # the re-check after signing in
            await pilot.press("enter")               # Enter opens the server's action menu
            assert type(app.screen).__name__ == "Menu"
            await pilot.click(offset=(0, 0))         # clicking the backdrop closes it
            assert type(app.screen).__name__ != "Menu"
            await pilot.click(app.cards["first"])    # clicking a card selects it and opens its menu
            assert app.selected.name == "first" and type(app.screen).__name__ == "Menu"
            await pilot.press("escape", "right")
            await pilot.click("#update-all")         # the big button opens the update-all menu
            assert type(app.screen).__name__ == "Menu" and "All servers" in str(app.screen.heading)
            await pilot.press("escape")
            updates = [l for l in (root / "calls").read_text().splitlines() if " update " in l]
            assert updates[0].startswith("second sh -s -- update ") and updates[0].endswith(" t3"), updates
            app.save_screenshot("/tmp/t3up-wide.svg")
            app.hosts.append(m["Host"]("third"))     # grid navigation on a 2-column, 2-row layout
            await app.grid.mount(app.cards.setdefault("third", m["Card"](app.hosts[-1])))
            await pilot.press("down")
            assert app.selected.name == "third"
            await pilot.press("up")
            assert app.selected.name == "first"
            assert app.cards["first"].styles.border_top[1].hex == m["ACCENT"].upper()
            assert app.cards["second"].styles.border_top[1].hex != m["ACCENT"].upper()
            await pilot.press("right", "down")       # second has nothing below -> clamps to last
            assert app.selected.name == "third"
            await pilot.resize_terminal(60, 24)
            await pilot.pause()
            assert app.grid.styles.grid_size_columns == 1
            await pilot.press("l")
            assert app.query_one("RichLog").has_class("shown")
            app.save_screenshot(str(root / "shot.svg"))
            os.system(f"cp {root}/shot.svg /tmp/t3up-shot.svg")

            # Servers editor: rejects bad hosts, edits only the changed lines (comments survive),
            # keeps state of kept hosts, checks new ones.
            kept = app.hosts[1]
            await pilot.press("e")
            await pilot.pause()
            await pilot.press(*"bad;host", "enter")
            assert (root / "servers").read_text() == "# test\n\nfirst # unavailable\nsecond", "invalid host saved"
            app.screen.query_one("Input").value = ""
            await pilot.press(*"fourth", "enter")
            await pilot.pause()
            remove = next(b for b in app.screen.query("Button.remove") if b.name == "first")
            await pilot.click(remove)
            await pilot.pause()
            assert (root / "servers").read_text() == "# test\n\nsecond\nfourth\n"
            assert [h.name for h in app.hosts] == ["first", "second", "third"], "applied before closing"
            await pilot.click(offset=(0, 0))         # clicking outside closes and applies
            await app.workers.wait_for_complete()
            await pilot.pause()
            assert [h.name for h in app.hosts] == ["second", "fourth"] and app.hosts[0] is kept
            assert app.hosts[1].status == "ok" and app.selected.name == "second"
    asyncio.run(drive())

    async def empty():  # no servers yet: dashboard opens straight into the editor
        app = m["T3Up"]([], root / "logs")
        async with app.run_test(size=(100, 30)) as pilot:
            await pilot.pause()
            assert type(app.screen).__name__ == "ServersEditor"
    asyncio.run(empty())

    # The real REMOTE script under dash, with fake tools first on its PATH ($HOME/.local/bin),
    # so no real installer on this machine can ever run.
    home = root / "home"
    bin_ = home / ".local/bin"
    bin_.mkdir(parents=True)
    def tool(name, body):
        (bin_ / name).write_text("#!/bin/sh\n" + body + "\n")
        (bin_ / name).chmod(0o755)
    # Only system dirs after the fakes: a missing fake must never fall through to a real tool.
    tool("ssh", 'while [ "$1" = -o ]; do shift 2; done; PATH=/usr/bin:/bin exec dash -c "$2"')
    tool("codex", 'case $1 in --version) echo "codex-cli $(cat $HOME/codex)";; login) [ -f $HOME/codex-auth ];; esac')
    tool("npm", 'sleep 1; head -c 100000 /dev/zero | tr "\\0" x; echo; echo "added 1 package"; echo 0.2.0 > $HOME/codex\n'
                'cp $HOME/codex-tool $HOME/.local/bin/codex')
    tool("claude", 'case $1 in --version) echo "$(cat $HOME/claude) (Claude Code)";;\n'
                   'update) sleep 1; [ -f $HOME/claude-broken ] && { echo "disk full" >&2; exit 3; }\n'
                   '  echo 2.0.0 > $HOME/claude;;\n'
                   'auth) [ -f $HOME/claude-auth ] && echo \'{ "loggedIn": true }\' || echo \'{ "loggedIn": false }\';; esac')
    for name in ("codex", "claude"):
        shutil.copy(bin_ / name, home / f"{name}-tool")
    tool("t3", 'case $1 in --version) echo x >> $HOME/t3-calls; echo "t3 v$(cat $HOME/t3)";;\n'
               'update) sleep 1; echo 0.0.2 > $HOME/t3;; esac')
    # curl -o writes a fake Claude installer (the health check's -o /dev/null is harmless).
    tool("curl", 'while [ $# -gt 0 ]; do [ "$1" = -o ] && printf \'%s\\n\' \'cp "$HOME/claude-tool" "$HOME/.local/bin/claude"\' \'echo 2.0.0 > "$HOME/claude"\' > "$2"; shift; done; exit 0')
    tool("systemctl", "exit 1")
    def reset_tools():
        for name, value in (("codex", "0.1.0"), ("claude", "1.0.0"), ("t3", "0.0.1")):
            (home / name).write_text(value)
        for name in ("t3-calls", "claude-broken"):
            (home / name).unlink(missing_ok=True)
        for name in ("codex", "claude"):
            (home / f"{name}-auth").touch()
            shutil.copy(home / f"{name}-tool", bin_ / name)
    os.environ.update(HOME=str(home), PATH=f"{bin_}:{os.environ['PATH']}")

    async def remote(mode, only="all"):
        h, events = m["Host"]("box"), []
        await m["run_job"](h, mode, "", "", only, root / "remote-logs", lambda h, k, t: events.append((k, t)))
        return h

    reset_tools()
    h = asyncio.run(remote("check"))
    assert h.status == "ok", (h.error, list(h.lines))
    assert (home / "t3-calls").read_text().count("x") == 1, "check must call t3 --version once"
    assert [h.steps[s][1] for s in ("Codex", "Claude", "T3")] == ["0.1.0", "1.0.0", "0.0.1"]

    reset_tools()
    started = time.monotonic()
    h = asyncio.run(remote("update"))
    assert h.status == "ok", (h.error, list(h.lines))
    assert time.monotonic() - started < 2.5, "components did not update in parallel"
    assert [h.steps[s][1] for s in ("Codex", "Claude", "T3")] == ["0.1.0 → 0.2.0", "1.0.0 → 2.0.0", "0.0.1 → 0.0.2"]
    assert "Codex: added 1 package" in h.lines, list(h.lines)
    assert max(map(len, h.lines)) < 4000, "long lines must be cut below PIPE_BUF"

    reset_tools()
    h = asyncio.run(remote("update", "codex"))
    assert [h.steps[s][1] for s in ("Codex", "Claude", "T3")] == ["0.1.0 → 0.2.0", "1.0.0", "0.0.1"]

    reset_tools()
    (home / "claude-broken").touch()
    h = asyncio.run(remote("update"))
    assert h.status == "failed" and h.steps["Claude"] == ("fail", "disk full"), h.steps
    assert h.steps["Codex"][0] == "done" and h.steps["T3"][0] == "done" and "Health" not in h.steps
    assert h.error == "Claude: disk full", h.error

    # Missing tools: check reports them, an update installs them; signed-out tools are reported.
    reset_tools()
    for name in ("codex", "claude"):
        (bin_ / name).unlink()
    h = asyncio.run(remote("check"))
    assert h.steps["Codex"] == ("skip", "not installed") and h.steps["Claude"][0] == "skip", h.steps
    h = asyncio.run(remote("update", "codex,claude"))
    assert h.status == "ok", (h.error, list(h.lines))
    assert [h.steps[s][1] for s in ("Codex", "Claude")] == ["new → 0.2.0", "new → 2.0.0"], h.steps
    assert h.auth == []
    (home / "claude-auth").unlink()
    h = asyncio.run(remote("check"))
    assert h.auth == ["Claude"], h.auth
print("PASS: parallel hosts and components, remote script under dash, argument validation, TUI, servers editor")
