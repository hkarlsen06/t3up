mode=$1 version=$2 only=${3:-all}
picked() { case ",$only," in *",$1,"*) true ;; *) false ;; esac; }
# Update a component only in update mode and when it was picked or ONLY is `all`.
want() { [ "$mode" = update ] && { picked all || picked "$1"; }; }
event() { printf '\n@@t3up\t%s\t%s\n' "$1" "$2"; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# One npm install at a time: parallel `npm install -g` into one prefix can corrupt it.
# npm is installation tooling, not an update target: keep the host's working version.
npm_into() {  # prefix package [verb: install]
  waited=0
  until mkdir "$tmp/npm-lock" 2>/dev/null; do  # its holder may have been killed
    waited=$((waited + 1))
    [ "$waited" -lt 900 ] || { echo 'Gave up waiting 15 minutes for another npm install' >&2; return 1; }
    sleep 1
  done
  if [ -w "$1" ]; then set -- npm "${3:-install}" -g --prefix "$1" "$2"
  else set -- sudo -n npm "${3:-install}" -g --prefix "$1" "$2"; fi
  if "$@"; then rmdir "$tmp/npm-lock"; else s=$?; rmdir "$tmp/npm-lock"; return "$s"; fi
}
# Update a provider CLI the way T3 Code's own provider updater does: with the tool's
# native updater when it installed itself, else with the package manager that owns it.
# Where a tool's command really lives ($real) and the npm package that owns it ($pkg).
owner() {  # bin npm-package
  path=$(command -v "$1")
  real=$(readlink -f "$path" 2>/dev/null || printf '%s' "$path")
  pkg=$2  # the package that owns the path wins: OpenCode 1.x and 2.x are different packages
  case $real in */node_modules/*)
    pkg=${real##*/node_modules/}  # @scope/name/bin/x or name/bin/x
    case $pkg in @*/*/*) scope=${pkg%%/*} pkg=${pkg#*/} pkg=$scope/${pkg%%/*} ;; *) pkg=${pkg%%/*} ;; esac
  esac
}
upgrade() {  # Name bin npm-package native-real-path-glob native-update-args
  owner "$2" "$3"
  case $real in $4) "$2" $5; return ;; esac
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
# Remove a provider the way it was installed. Its settings and sign-in stay, so the Installer can
# put it back as it was. A package manager's install goes through it; a tool's own install (the glob)
# is taken apart by `<bin>_remove`, last, since Grok's glob matches anything.
uninstall() {  # Name bin npm-package native-real-path-glob
  owner "$2" "$3"
  case $real in
    */.bun/*) bun remove -g "$pkg" ;;
    */pnpm/*) pnpm remove -g "$pkg" ;;
    */lib/node_modules/*) prefix=${real%%/lib/node_modules/*}; npm_into "${prefix:-/}" "$pkg" uninstall ;;
    */Cellar/*|*/Caskroom/*)
      keg=${real#*/Cellar/} keg=${keg#*/Caskroom/}
      case $real in */Caskroom/*) brew uninstall --cask "${keg%%/*}" ;; *) brew uninstall "${keg%%/*}" ;; esac ;;
    $4) "$2_remove" ;;
    *) echo "$1 at $real was not installed by npm, bun, pnpm, Homebrew or its own installer; remove it by hand" >&2
       return 1 ;;
  esac
}
# A link of ours in $1 that points into $2: remove it (never someone else's file).
unlink_into() {  # link dir
  case $(readlink "$1" 2>/dev/null) in "$2"/*) rm -f "$1" ;; esac
}
codex_remove() {  # ~/.codex keeps its config and sign-in
  rm -rf "${CODEX_HOME:-$HOME/.codex}/packages/standalone"
  unlink_into "$path" "${CODEX_HOME:-$HOME/.codex}"
  rm -f "$HOME/.local/bin/codex-code-mode-host"
}
claude_remove() {  # ~/.claude keeps its settings and sign-in
  rm -rf "$HOME/.local/share/claude"
  unlink_into "$path" "$HOME/.local/share/claude"
}
opencode_remove() {  # its uninstaller clears caches, then leaves the binary for you to delete
  opencode uninstall --keep-config --keep-data --force </dev/null
  rm -f "$HOME/.opencode/bin/opencode"
  rmdir "$HOME/.opencode/bin" "$HOME/.opencode" 2>/dev/null || true
}
grok_remove() {  # its installer linked grok and agent; ~/.grok keeps the rest
  for link in "$HOME/.local/bin/grok" "$HOME/.local/bin/agent"; do unlink_into "$link" "$HOME/.grok/bin"; done
  rm -rf "$HOME/.grok/bin"
}
pi_remove() {  # ~/.pi/agent keeps its settings
  unlink_into "$path" "$HOME/.pi/agent"
  rm -rf "$HOME/.pi/agent/install" "$HOME/.pi/agent/bin"
}
# A Node.js for tools that need one (Pi): the official LTS build for this machine, checked against
# its published SHA-256, per user, no sudo. It goes where Pi's own installer puts a standalone Node.
ensure_node() {
  if command -v npm >/dev/null && node -e 'const [a, b] = process.versions.node.split(".").map(Number); process.exit(a > 22 || (a === 22 && b >= 19) ? 0 : 1)' 2>/dev/null; then
    return 0
  fi
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) plat=linux-x64 ;;
    Linux-aarch64|Linux-arm64) plat=linux-arm64 ;;
    Darwin-arm64) plat=darwin-arm64 ;;
    Darwin-x86_64) plat=darwin-x64 ;;
    *) echo "No Node.js build for $(uname -sm); install Node.js 22.19 or newer" >&2; return 1 ;;
  esac
  base=${T3UP_NODE_DIST:-https://nodejs.org/dist/latest-v24.x}
  echo "Installing Node.js for $plat from $base"
  curl -fsSL "$base/SHASUMS256.txt" -o "$tmp/node-sums"
  file=$(awk -v want="-$plat.tar.gz" 'substr($2, length($2) - length(want) + 1) == want { print $2; exit }' "$tmp/node-sums")
  [ -n "$file" ] || { echo "No Node.js $plat build is listed" >&2; return 1; }
  curl -fsSL "$base/$file" -o "$tmp/$file"
  want=$(awk -v f="$file" '$2 == f { print $1 }' "$tmp/node-sums")
  got=$( { sha256sum "$tmp/$file" 2>/dev/null || shasum -a 256 "$tmp/$file"; } | awk '{ print $1 }')
  [ -n "$want" ] && [ "$want" = "$got" ] || { echo 'The Node.js download does not match its checksum' >&2; return 1; }
  dir=${XDG_DATA_HOME:-$HOME/.local/share}/pi-node
  mkdir -p "$dir"
  tar -xzf "$tmp/$file" -C "$dir"
  ln -sfn "$dir/${file%.tar.gz}" "$dir/current"
  PATH="$dir/current/bin:$PATH"; export PATH
}
# Report a provider's version, updating it when wanted. A missing tool is installed per user
# (no sudo) with its official installer, only when picked by name; otherwise its step ends here,
# as skipped. The installers never prompt here: no terminal, and NON_INTERACTIVE for Codex's.
tool() {  # Name bin npm-package native-real-path-glob native-update-args [install-script-url]
  if ! command -v "$2" >/dev/null; then
    if [ "$mode" = update ] && picked "$2"; then
      if [ -n "${6:-}" ]; then
        curl -fsSL "$6" -o "$tmp/$2-install.sh"
        NON_INTERACTIVE=1 bash "$tmp/$2-install.sh" </dev/null
      else mkdir -p "$HOME/.local"; npm_into "$HOME/.local" "$3@latest"; fi
      event done "$1: none -> $("$2" --version </dev/null)"
      return
    fi
    event skip "$1: not installed"
    exit 0  # leaves the step's subshell: nothing else to check
  fi
  before=$("$2" --version </dev/null) after=$before
  if [ "$mode" = remove ] && picked "$2"; then
    uninstall "$@"
    hash -r 2>/dev/null || true
    if command -v "$2" >/dev/null; then echo "$1 is still on the PATH at $(command -v "$2")" >&2; return 1; fi
    event skip "$1: removed"
    exit 0  # nothing left to sign in to
  fi
  if want "$2"; then upgrade "$@"; after=$("$2" --version </dev/null); fi
  event done "$1: $before -> $after"
}
# Each provider step then reports whether the tool is signed in; t3up runs the login itself.
codex_step() {
  tool Codex codex @openai/codex '*/packages/standalone/*' update https://chatgpt.com/codex/install.sh
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
  tool Grok grok @xai-official/grok '*' update https://x.ai/cli/install.sh
  grok models </dev/null 2>&1 | grep -qi 'you are logged in' || event auth Grok
}
pi_step() {  # API keys per model provider; no sign-in to check. Its installer needs Node to be there.
  if ! command -v pi >/dev/null && [ "$mode" = update ] && picked pi; then ensure_node; fi
  tool Pi pi @earendil-works/pi-coding-agent '*/.pi/agent/*' update https://pi.dev/install.sh
}
t3_binary() {
  state="$HOME/.t3/runtime/service-state.json"
  if [ -f "$state" ] && systemctl --user cat t3code.service >/dev/null 2>&1; then
    # Not node: the official installer brings none.
    active=$(sed -n 's/.*"activeVersion"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$state")
    case $active in ''|[!0-9]*|*[!a-zA-Z0-9.+-]*) echo 'Unreadable T3 service state' >&2; return 1 ;; esac
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
  cpus=$(getconf _NPROCESSORS_ONLN 2>/dev/null) || cpus=""
  [ -z "$cpus" ] || info="${info}${info:+ · }cpus $cpus"
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
# A server without T3 gets it the official way: the installer on the nightly train (or the pinned
# version), then the per-user background service, which needs lingering to outlive logins.
t3_fresh() {
  # T3's Linux build links libatomic, which minimal installs lack: add it when sudo needs no password.
  if [ "$(uname -s)" = Linux ] && ! { ldconfig -p 2>/dev/null || /sbin/ldconfig -p 2>/dev/null; } | grep -q 'libatomic\.so\.1'; then
    if command -v apt-get >/dev/null && sudo -n true 2>/dev/null; then
      echo 'Installing libatomic1, which T3 needs'
      sudo -n sh -c 'apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libatomic1' >/dev/null
    elif command -v dnf >/dev/null && sudo -n true 2>/dev/null; then
      echo 'Installing libatomic, which T3 needs'
      sudo -n dnf install -y -q libatomic >/dev/null
    else
      echo 'T3 needs libatomic (on Ubuntu: sudo apt install libatomic1); install it and try again' >&2
      return 1
    fi
  fi
  curl -fsSL https://t3.codes/install.sh -o "$tmp/t3-install.sh"
  if [ -n "$1" ]; then T3CODE_VERSION=$1 sh "$tmp/t3-install.sh"
  else T3CODE_CHANNEL=nightly sh "$tmp/t3-install.sh"; fi
  if command -v loginctl >/dev/null && ! loginctl show-user "$(id -un)" -p Linger 2>/dev/null | grep -q '=yes'; then
    sudo -n loginctl enable-linger "$(id -un)" 2>/dev/null ||
      echo 'Could not enable lingering without a password; T3 may stop when you log out' >&2
  fi
  "$HOME/.local/bin/t3" service install || {
    echo "T3 is installed, but its background service didn't start: see t3 service status on $(hostname)" >&2
    return 1
  }
}
# Cursor and Antigravity run inside T3 (an SDK and a T3-managed download): updating T3 updates them.
t3_step() {
  event busy "$(busy)"
  if ! command -v t3 >/dev/null && [ ! -f "$HOME/.t3/runtime/service-state.json" ]; then
    want t3 || { event skip 'T3: not installed'; exit 0; }
    t3_fresh "$version"
    binary=$(t3_binary)
    server=$("$binary" --version)
    server=$(printf '%s\n' "$server" | sed 's/^t3 v//')
    : > "$tmp/t3-before"
    : > "$tmp/t3-fresh"  # a first install has nothing to roll back to
    printf '%s\n' "$server" > "$tmp/t3-after"
    event version "$server"
    event done "T3: none -> $server"
    event auth T3  # a new server pairs with your T3 Code app
    return
  fi
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
# Health, when T3 is there to answer (a server without it skipped its step).
if [ "$(cat "$tmp/T3" 2>/dev/null)" = 0 ] && [ -f "$tmp/t3-after" ]; then
  event begin 'Health'
  if health; then event done 'Health: OK'
  else
    before=$(cat "$tmp/t3-before") after=$(cat "$tmp/t3-after")
    if [ "$mode" = update ] && [ ! -f "$tmp/t3-fresh" ] && [ "$before" != "$after" ]; then
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
