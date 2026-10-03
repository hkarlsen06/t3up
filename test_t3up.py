#!/Users/hkarlsen06/.local/share/t3up/venv/bin/python
"""Run with ./test_t3up.py; never contacts a server (fake ssh on PATH)."""
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
printf '@@t3up\\tversion\\t1.0.0\\n@@t3up\\tdone\\tHealth OK\\n@@t3up\\tcomplete\\tOK\\n'
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

    async def drive():
        app = m["T3Up"](["first", "second"], root / "logs")
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
            await pilot.press("enter")               # Enter opens the server's action menu
            assert type(app.screen).__name__ == "Menu"
            await pilot.click(offset=(0, 0))         # clicking the backdrop closes it
            assert type(app.screen).__name__ != "Menu"
            await pilot.click(app.cards["first"])    # clicking a card selects it and opens its menu
            assert app.selected.name == "first" and type(app.screen).__name__ == "Menu"
            await pilot.press("escape", "right")
            last = (root / "calls").read_text().splitlines()[-1]
            assert last.startswith("second sh -s -- update ") and last.endswith(" t3"), last
            app.save_screenshot("/tmp/t3up-wide.svg")
            app.hosts.append(m["Host"]("third"))     # grid navigation on a 2-column, 2-row layout
            await app.grid.mount(app.cards.setdefault("third", m["Card"](app.hosts[-1])))
            await pilot.press("down")
            assert app.selected.name == "third"
            await pilot.press("up")
            assert app.selected.name == "first"
            assert app.cards["first"].styles.border_top[1].hex == "#F59E0B"
            assert app.cards["second"].styles.border_top[1].hex != "#F59E0B"
            await pilot.press("right", "down")       # second has nothing below -> clamps to last
            assert app.selected.name == "third"
            await pilot.resize_terminal(60, 24)
            await pilot.pause()
            assert app.grid.styles.grid_size_columns == 1
            await pilot.press("l")
            assert app.query_one("RichLog").has_class("shown")
            app.save_screenshot(str(root / "shot.svg"))
            os.system(f"cp {root}/shot.svg /tmp/t3up-shot.svg")

            # Servers editor: rejects bad hosts, keeps state of kept hosts, checks new ones.
            await pilot.press("e")
            editor = app.screen.query_one("TextArea")
            editor.text = "bad;host"
            await pilot.press("ctrl+s")
            assert (root / "servers").read_text().endswith("second"), "invalid list was saved"
            editor.text = "second\nfourth # new"
            await pilot.click("#save")
            await app.workers.wait_for_complete()
            await pilot.pause()
            assert (root / "servers").read_text() == "second\nfourth # new\n"
            assert [h.name for h in app.hosts] == ["second", "fourth"] and app.hosts[0].mode == "update"
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
    tool("ssh", 'while [ "$1" = -o ]; do shift 2; done; exec dash -c "$2"')
    tool("codex", '[ "$1" = --version ] && echo "codex-cli $(cat $HOME/codex)"')
    tool("npm", 'sleep 1; head -c 100000 /dev/zero | tr "\\0" x; echo; echo "added 1 package"; echo 0.2.0 > $HOME/codex')
    tool("claude", 'case $1 in --version) echo "$(cat $HOME/claude) (Claude Code)";;\n'
                   'update) sleep 1; [ -f $HOME/claude-broken ] && { echo "disk full" >&2; exit 3; }\n'
                   '  echo 2.0.0 > $HOME/claude;; esac')
    tool("t3", 'case $1 in --version) echo x >> $HOME/t3-calls; echo "t3 v$(cat $HOME/t3)";;\n'
               'update) sleep 1; echo 0.0.2 > $HOME/t3;; esac')
    tool("curl", "exit 0")
    tool("systemctl", "exit 1")
    def reset_tools():
        for name, value in (("codex", "0.1.0"), ("claude", "1.0.0"), ("t3", "0.0.1")):
            (home / name).write_text(value)
        for name in ("t3-calls", "claude-broken"):
            (home / name).unlink(missing_ok=True)
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
print("PASS: parallel hosts and components, remote script under dash, argument validation, TUI, servers editor")
