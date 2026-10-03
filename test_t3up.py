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
    # A fake npm registry on disk: Codex has a newer release than the fake server's 1.1.0.
    tags = root / "registry/-/package/@openai/codex/dist-tags"
    tags.parent.mkdir(parents=True)
    tags.write_text('{"latest": "1.2.0"}')
    os.environ.update(T3UP_REGISTRY=(root / "registry").as_uri())
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
    assert "update available: Codex 1.2.0" in result.stdout, result.stdout
    assert "\033" not in result.stdout
    assert "! Codex not signed in; run: ssh -t second 'for dir in" in result.stdout, result.stdout
    (root / "calls").unlink()
    result = subprocess.run([command, "bad'; touch /tmp/no"], capture_output=True)
    assert result.returncode == 2 and not (root / "calls").exists()
    assert subprocess.run([command, "--check", "--only", "t3"], capture_output=True).returncode == 2
    subprocess.run([command, "--only", "codex", "--host", "second"], capture_output=True)
    assert (root / "calls").read_text().split("--", 1)[1].split()[::2] == ["update", "codex"]
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
                try: out = os.read(fd, 65536)
                except OSError: return
                if b"\x1b[c" in out:  # answer the logo probe's device query, as a terminal does
                    os.write(fd, b"\x1b[?62;22c")
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
    assert m["version_text"](n + "2623 -> t3 0.0.46-nightly.20261003.2632") == "0.0.46-nightly (…23 → …32)", "no fake downgrade"
    from rich.console import Console
    for name, logo in m["load_logos"]().items():  # no terminal here: the half-block fallback
        lines = Console(width=40).render_lines(logo, pad=False)
        assert len(lines) == m["LOGO_LINES"] and 0 < max(sum(s.cell_length for s in l) for l in lines) <= 2 * m["TILE"], name
    assert m["compact"]("0.0.46-nightly.20261003.2632", "0.0.46-nightly.20261003.2623") == "#2632"
    assert m["compact"]("0.0.47-nightly.20261004.2701", "0.0.46-nightly.20261003.2623") == "0.0.47"
    box = m["Host"]("box")
    box.current = {"Codex": "1.2.0", "T3": "0.0.46-nightly.20261003.2632", "OpenCode": "2.0.1"}
    latest = {"Codex": "1.2.0", "T3": "0.0.46-nightly.20261003.2632", "OpenCode": "1.18.34", "OpenCode 2": "2.0.22"}
    assert m["updates"](box, latest) == {"OpenCode": "2.0.22"}, "only newer releases, in the installed package"
    assert m["updates"](box, latest, "0.0.45") == {"OpenCode": "2.0.22", "T3": "0.0.45"}, "a pinned T3 target wins"
    assert m["desktop_behind"]("0.0.46-nightly.20261003.2623", [box]) == "0.0.46-nightly.20261003.2632"
    assert m["desktop_behind"]("0.0.46-nightly.20261003.2632", [box]) == "" == m["desktop_behind"]("", [box])
    assert m["versions_tooltip"](box, {"OpenCode": "2.0.22"}).splitlines() == [
        "T3        0.0.46-nightly.20261003.2632", "Codex     1.2.0", "OpenCode  2.0.1  →  2.0.22"]

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
            assert m["updates"](app.hosts[1], app.latest) == {"Codex": "1.2.0"}, app.latest
            assert "1 to update" in str(app.cards["second"].border_subtitle)
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

    async def narrow():  # an outdated desktop app stays in the header, just shorter
        from rich.console import Console
        app = m["T3Up"](["box"], root / "logs")
        app.start = lambda *a, **k: None
        async with app.run_test(size=(70, 20)) as pilot:
            await pilot.pause()
            app.desktop, app.hosts[0].current = "0.0.46-nightly.20261003.2623", {"T3": "0.0.46-nightly.20261003.2632"}
            app.tick()
            console = Console(width=app.top.size.width, record=True)
            console.print(app.top.content)
            assert "update desktop → #2632" in console.export_text(), console.export_text()
    asyncio.run(narrow())

    async def empty():  # no servers yet: dashboard opens straight into the editor
        app = m["T3Up"]([], root / "logs")
        async with app.run_test(size=(100, 30)) as pilot:
            await pilot.pause()
            assert type(app.screen).__name__ == "ServersEditor"
    asyncio.run(empty())

    # The real REMOTE script under dash, with fake tools first on its PATH ($HOME/.local/bin),
    # so no real installer on this machine can ever run. Each fake lives where a real install
    # puts it, since that decides how the tool is updated.
    home = root / "home"
    bin_ = home / ".local/bin"
    bin_.mkdir(parents=True)
    def tool(name, body, path=None):
        path = home / (path or f".local/bin/{name}")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/sh\n" + body + "\n")
        path.chmod(0o755)
    # Only system dirs: a missing fake must never fall through to a real tool. REMOTE adds
    # /usr/local/bin and /opt/homebrew/bin, where this Mac has real ones, so drop those too.
    tool("ssh", 'while [ "$1" = -o ]; do shift 2; done; PATH=/usr/bin:/bin exec dash -c "$2"')
    globals_ = m["run_job"].__globals__
    globals_["REMOTE"] = globals_["REMOTE"].replace(":/usr/local/bin:/opt/homebrew/bin:", ":")
    assert "/opt/homebrew" not in globals_["REMOTE"]
    codex_js = ".local/lib/node_modules/@openai/codex/bin/codex.js"
    pi_js = ".local/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js"
    claude_bin = ".local/share/claude/versions/1/claude"
    tool("codex", 'case $1 in --version) echo "codex-cli $(cat $HOME/codex)";; login) [ -f $HOME/codex-auth ];; esac', codex_js)
    tool("pi", 'echo "$(cat $HOME/pi)"', pi_js)
    tool("claude", 'case $1 in --version) echo "$(cat $HOME/claude) (Claude Code)";;\n'
                   'update) sleep 1; [ -f $HOME/claude-broken ] && { echo "disk full" >&2; exit 3; }\n'
                   '  echo 2.0.0 > $HOME/claude;;\n'
                   'auth) [ -f $HOME/claude-auth ] && echo \'{ "loggedIn": true }\' || echo \'{ "loggedIn": false }\';; esac',
         claude_bin)
    tool("grok", 'case $1 in --version) echo "grok $(cat $HOME/grok)";; update) echo 0.4.0 > $HOME/grok;;\n'
                 'models) [ -f $HOME/grok-auth ] && echo "You are logged in with grok.com." || echo "Not logged in";; esac')
    # npm installs whichever package it's given, as a real npm would, and logs its arguments.
    tool("npm", 'echo "$*" >> $HOME/npm-calls; sleep 1; head -c 100000 /dev/zero | tr "\\0" x; echo; echo "added 1 package"\n'
                'case "$*" in *@openai/codex@*) echo 0.2.0 > $HOME/codex; ln -sf $HOME/' + codex_js + ' $HOME/.local/bin/codex;;\n'
                '*pi-coding-agent@*) echo 0.6.0 > $HOME/pi; ln -sf $HOME/' + pi_js + ' $HOME/.local/bin/pi;; esac')
    tool("t3", 'case $1 in --version) echo x >> $HOME/t3-calls; echo "t3 v$(cat $HOME/t3)";;\n'
               'update) sleep 1; echo 0.0.2 > $HOME/t3;; esac')
    # curl -o writes a fake Claude installer (the health check's -o /dev/null is harmless).
    tool("curl", 'while [ $# -gt 0 ]; do [ "$1" = -o ] && printf \'%s\\n\' \'ln -sf "$HOME/' + claude_bin + '" "$HOME/.local/bin/claude"\' \'echo 2.0.0 > "$HOME/claude"\' > "$2"; shift; done; exit 0')
    tool("systemctl", "exit 1")
    def reset_tools():
        for name, value in (("codex", "0.1.0"), ("claude", "1.0.0"), ("grok", "0.3.0"), ("pi", "0.5.0"), ("t3", "0.0.1")):
            (home / name).write_text(value)
        for name in ("t3-calls", "claude-broken", "npm-calls", ".local/bin/pi"):
            (home / name).unlink(missing_ok=True)
        for name, path in (("codex", codex_js), ("claude", claude_bin)):
            (home / f"{name}-auth").touch()
            (bin_ / name).unlink(missing_ok=True)
            (bin_ / name).symlink_to(home / path)
        (home / "grok-auth").touch()
    os.environ.update(HOME=str(home), PATH=f"{bin_}:{os.environ['PATH']}")

    async def remote(mode, only="all"):
        h, events = m["Host"]("box"), []
        await m["run_job"](h, mode, "", only, root / "remote-logs", lambda h, k, t: events.append((k, t)))
        return h
    steps = lambda h, *names: [h.steps[s][1] for s in names]

    reset_tools()
    h = asyncio.run(remote("check"))
    assert h.status == "ok", (h.error, list(h.lines))
    assert (home / "t3-calls").read_text().count("x") == 1, "check must call t3 --version once"
    assert steps(h, "Codex", "Claude", "Grok", "T3") == ["0.1.0", "1.0.0", "0.3.0", "0.0.1"]
    assert h.steps["OpenCode"] == h.steps["Pi"] == ("skip", "not installed"), h.steps

    reset_tools()
    started = time.monotonic()
    h = asyncio.run(remote("update"))
    assert h.status == "ok", (h.error, list(h.lines))
    assert h.steps["Pi"][0] == "skip", "updating everything must not install missing providers"
    assert time.monotonic() - started < 2.5, "components did not update in parallel"
    assert steps(h, "Codex", "Claude", "Grok", "T3") == ["0.1.0 → 0.2.0", "1.0.0 → 2.0.0", "0.3.0 → 0.4.0", "0.0.1 → 0.0.2"]
    assert (home / "npm-calls").read_text() == f"install -g --prefix {home.resolve()}/.local @openai/codex@latest\n", (home / "npm-calls").read_text()
    assert "Codex: added 1 package" in h.lines, list(h.lines)
    assert max(map(len, h.lines)) < 4000, "long lines must be cut below PIPE_BUF"

    reset_tools()
    h = asyncio.run(remote("update", "codex"))
    assert steps(h, "Codex", "Claude", "T3") == ["0.1.0 → 0.2.0", "1.0.0", "0.0.1"]

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
    started = time.monotonic()
    h = asyncio.run(remote("update", "codex,claude,pi"))
    assert h.status == "ok", (h.error, list(h.lines))
    assert time.monotonic() - started > 2, "npm installs must not overlap"
    assert steps(h, "Codex", "Claude", "Pi") == ["new → 0.2.0", "new → 2.0.0", "new → 0.6.0"], h.steps
    assert h.auth == []
    # A second update finds Pi through its npm prefix and the package that owns it.
    (home / "npm-calls").unlink()
    h = asyncio.run(remote("update", "pi"))
    assert (home / "npm-calls").read_text() == f"install -g --prefix {home.resolve()}/.local @earendil-works/pi-coding-agent@latest\n"
    # pnpm's shim is a script, not a link into node_modules: the known package is used.
    (bin_ / "pi").unlink()
    tool("pi", 'echo 0.5.0', ".local/share/pnpm/pi")
    tool("pnpm", 'echo "$*" >> $HOME/npm-calls')
    (home / "npm-calls").unlink()
    h = asyncio.run(remote("update", "pi"))
    assert (home / "npm-calls").read_text() == "add -g @earendil-works/pi-coding-agent@latest\n", h.lines
    # `all` inside a list still means everything installed.
    h = asyncio.run(remote("update", "all,pi"))
    assert h.steps["Grok"][1] == "0.3.0 → 0.4.0", h.steps
    (home / "claude-auth").unlink()
    (home / "grok-auth").unlink()
    h = asyncio.run(remote("check"))
    assert sorted(h.auth) == ["Claude", "Grok"], h.auth
    # A tool installed some other way fails with a reason instead of guessing.
    (bin_ / "codex").unlink()
    tool("codex", 'echo "codex-cli 0.1.0"')
    h = asyncio.run(remote("update", "codex"))
    assert h.steps["Codex"][0] == "fail" and "update it by hand" in h.error, h.error
print("PASS: parallel hosts and components, remote script under dash, argument validation, TUI, servers editor")
