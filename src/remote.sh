mode=$1 version=$2 only=${3:-all}
picked() { case ",$only," in *",$1,"*) true ;; *) false ;; esac; }
# Update a component only in update mode and when it was picked or ONLY is `all`.
want() { [ "$mode" = update ] && { picked all || picked "$1"; }; }
event() { printf '\n@@t3up\t%s\t%s\n' "$1" "$2"; }
command -v t3 >/dev/null
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# One npm install at a time: parallel `npm install -g` into one prefix can corrupt it.
# npm is installation tooling, not an update target: keep the host's working version.
npm_into() {  # prefix package
  waited=0
  until mkdir "$tmp/npm-lock" 2>/dev/null; do  # its holder may have been killed
    waited=$((waited + 1))
    [ "$waited" -lt 900 ] || { echo 'Gave up waiting 15 minutes for another npm install' >&2; return 1; }
    sleep 1
  done
  if [ -w "$1" ]; then set -- npm install -g --prefix "$1" "$2"
  else set -- sudo -n npm install -g --prefix "$1" "$2"; fi
  if "$@"; then rmdir "$tmp/npm-lock"; else s=$?; rmdir "$tmp/npm-lock"; return "$s"; fi
}
# Update a provider CLI the way T3 Code's own provider updater does: with the tool's
# native updater when it installed itself, else with the package manager that owns it.
upgrade() {  # Name bin npm-package native-real-path-glob native-update-args
  real=$(command -v "$2")
  real=$(readlink -f "$real" 2>/dev/null || printf '%s' "$real")
  case $real in $4) "$2" $5; return ;; esac
  pkg=$3  # the package that owns the path wins: OpenCode 1.x and 2.x are different packages
  case $real in */node_modules/*)
    pkg=${real##*/node_modules/}  # @scope/name/bin/x or name/bin/x
    case $pkg in @*/*/*) scope=${pkg%%/*} pkg=${pkg#*/} pkg=$scope/${pkg%%/*} ;; *) pkg=${pkg%%/*} ;; esac
  esac
  case $real in
    */.bun/*) bun add -g "$pkg@latest" ;;
    */pnpm/*) pnpm add -g "$pkg@latest" ;;
    */lib/node_modules/*) prefix=${real%%/lib/node_modules/*}; npm_into "${prefix:-/}" "$pkg@latest" ;;
    */Cellar/*|*/Caskroom/*)
      keg=${real#*/Cellar/} keg=${keg#*/Caskroom/}
      case $real in */Caskroom/*) brew upgrade --cask "${keg%%/*}" ;; *) brew upgrade "${keg%%/*}" ;; esac ;;
    *) echo "$1 at $real was not installed by npm, bun, pnpm, Homebrew or its own installer; update it by hand" >&2
       return 1 ;;
  esac
}
# Report a provider's version, updating it when wanted. A missing tool is installed per
# user (no sudo) only when picked by name; otherwise its step ends here, as skipped.
tool() {  # Name bin npm-package native-real-path-glob native-update-args [install-script-url]
  if ! command -v "$2" >/dev/null; then
    if [ "$mode" = update ] && picked "$2"; then
      if [ -n "${6:-}" ]; then
        curl -fsSL "$6" -o "$tmp/$2-install.sh"
        bash "$tmp/$2-install.sh"
      else mkdir -p "$HOME/.local"; npm_into "$HOME/.local" "$3@latest"; fi
      event done "$1: none -> $("$2" --version </dev/null)"
      return
    fi
    event skip "$1: not installed"
    exit 0  # leaves the step's subshell: nothing else to check
  fi
  before=$("$2" --version </dev/null) after=$before
  if want "$2"; then upgrade "$@"; after=$("$2" --version </dev/null); fi
  event done "$1: $before -> $after"
}
# Each provider step then reports whether the tool is signed in; t3up runs the login itself.
codex_step() {
  tool Codex codex @openai/codex '*/packages/standalone/*' update
  codex login status </dev/null >/dev/null 2>&1 || event auth Codex
}
claude_step() {
  tool Claude claude @anthropic-ai/claude-code '*/.local/share/claude/*' update https://claude.ai/install.sh
  claude auth status --json </dev/null 2>/dev/null | grep -q '"loggedIn": *true' || event auth Claude
}
opencode_step() {  # signs in per model provider, so there is no single status to check
  tool OpenCode opencode opencode-ai '*/.opencode/bin/opencode' upgrade https://opencode.ai/install
}
grok_step() {  # Grok updates itself however it was installed
  tool Grok grok @xai-official/grok '*' update
  grok models </dev/null 2>&1 | grep -qi 'you are logged in' || event auth Grok
}
pi_step() {  # API keys per model provider; no sign-in to check
  tool Pi pi @earendil-works/pi-coding-agent '' ''
}
t3_binary() {
  state="$HOME/.t3/runtime/service-state.json"
  if [ -f "$state" ] && systemctl --user cat t3code.service >/dev/null 2>&1; then
    active=$(node -e 'const s=require(process.argv[1]); if (!/^[0-9][a-zA-Z0-9.+-]*$/.test(s.activeVersion)) process.exit(1); process.stdout.write(s.activeVersion)' "$state")
    binary="$HOME/.t3/runtime/versions/$active/t3"
    [ -x "$binary" ] || { echo 'Active T3 executable is missing' >&2; return 1; }
    printf '%s\n' "$binary"
  else command -v t3; fi
}
# ponytail: process names are a heuristic; unrecognized wrappers and ancestry deeper than 256 need explicit matching.
busy() {
  ps -eo pid=,ppid=,args= 2>/dev/null | awk '
    { pid=$1; parent[pid]=$2; $1=$2=""; args=$0
      server[pid]=(args ~ /(^|[ \/])t3 serve( |$)/)
      agent[pid]=(args ~ /(^|[ \/])(claude|codex|opencode|grok|pi)(\.js)?( |$)/ ||
        args ~ /node_modules\/(@openai\/codex|@anthropic-ai\/claude-code|opencode-ai|@opencode\/cli|@xai-official\/grok|@earendil-works\/pi-coding-agent)\//)
    }
    END { count=0
      for (pid in parent) if (agent[pid]) {
        p=parent[pid]; found=0; nested=0
        for (i=0; p in parent && i<256; i++) {
          if (agent[p]) { nested=1; break }
          if (server[p]) found=1
          p=parent[p]
        }
        if (found && !nested) count++
      }
      print count
    }' || printf '0\n'
}
system_info() {
  info=""
  if [ -r /proc/loadavg ]; then read -r load rest < /proc/loadavg
  else load=$(sysctl -n vm.loadavg 2>/dev/null | awk '{print $2}') || load=""; fi
  [ -z "$load" ] || info="load $load"
  disk=$(df -P "$HOME" 2>/dev/null | awk 'NR==2 {print $5}') || disk=""
  [ -z "$disk" ] || info="${info}${info:+ · }disk $disk"
  if [ -r /proc/uptime ]; then
    up=$(awk '{d=int($1/86400); h=int($1/3600)%24; if (d) printf "%dd %dh",d,h; else printf "%dh",h}' /proc/uptime)
    info="${info}${info:+ · }up $up"
  fi
  event sys "$info"
}
# Both the update and rollback use the existing install method.
t3_install() {
  if systemctl cat t3.service >/dev/null 2>&1; then
    npm_into "$(npm prefix -g)" "t3@${1:-nightly}"
    sudo -n systemctl restart t3.service
  elif [ -n "$1" ]; then "$binary" update --yes --allow-downgrade "$1"
  else "$binary" update --yes --channel nightly; fi
  binary=$(t3_binary)
  if [ "$binary" != "$(command -v t3)" ]; then
    mkdir -p "$HOME/.local/bin"
    ln -sfn "$binary" "$HOME/.local/bin/t3"
  fi
}
# Cursor and Antigravity run inside T3 (an SDK and a T3-managed download): updating T3 updates them.
t3_step() {
  event busy "$(busy)"
  binary=$(t3_binary)
  before=$("$binary" --version) || { echo 'T3 --version failed; update stopped' >&2; return 1; }
  before=$(printf '%s\n' "$before" | sed 's/^t3 v//') server=$before
  if want t3; then
    t3_install "$version"
    server=$("$binary" --version)
    server=$(printf '%s\n' "$server" | sed 's/^t3 v//')
  fi
  printf '%s\n' "$before" > "$tmp/t3-before"
  printf '%s\n' "$server" > "$tmp/t3-after"
  event version "$server"
  event done "T3: $before -> $server"
}
# Run a step in the background with errexit, tag its output, record its status.
# Lines are cut to 3000 bytes so each printf is one write under PIPE_BUF (4096):
# longer writes from parallel steps could interleave and split a @@t3up event.
step() {
  event begin "$1"
  ( set +e; ( set -e; "$2" ); echo $? > "$tmp/$1" ) 2>&1 |
    while IFS= read -r line || [ -n "$line" ]; do printf '%s| %.3000s\n' "$1" "$line"; done &
}
health() {
  for attempt in 1 2 3 4 5 6 7 8 9 10; do
    if curl --max-time 3 -fsS -o /dev/null http://127.0.0.1:3773/ 2>/dev/null; then return 0; fi
    sleep 1
  done
  echo 'T3 health check failed on localhost:3773' >&2
  return 1
}
rollback_step() {
  case $before in ''|[!0-9]*|*[!a-zA-Z0-9.+-]*)
    echo 'T3 rollback impossible: previous version is unknown or invalid' >&2
    return 1 ;;
  esac
  binary=$(t3_binary)
  t3_install "$before"
  health || { echo 'T3 rollback health check failed' >&2; return 1; }
  event version "$before"
  event rollback "T3: $after -> $before"
  event done 'Rollback: OK'
}
system_info || true
names='T3 Codex Claude OpenCode Grok Pi'
for name in $names; do step "$name" "$(printf '%s' "$name" | tr A-Z a-z)_step"; done
wait
failed=0
for name in $names; do
  if [ "$(cat "$tmp/$name" 2>/dev/null)" != 0 ]; then event fail "$name"; failed=1; fi
done
if [ "$(cat "$tmp/T3" 2>/dev/null)" = 0 ]; then
  event begin 'Health'
  if health; then event done 'Health: OK'
  else
    before=$(cat "$tmp/t3-before") after=$(cat "$tmp/t3-after")
    if [ "$mode" = update ] && [ "$before" != "$after" ]; then
      event begin Rollback
      ( set -e; rollback_step ) &
      if ! wait "$!"; then
        echo 'T3 rollback failed; server needs attention' >&2
        event fail Rollback
      fi
    fi
    exit 1
  fi
fi
[ "$failed" = 0 ] || exit 1
event complete OK
