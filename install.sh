#!/usr/bin/env bash
# ============================================================================
# claudebase installer
# ============================================================================
#
# Installs the `claudebase` CLI binary and the associated agent toolkit
# (rules, commands, agents) into `~/.claude/`. Designed to be invoked
# either standalone (one-shot from anywhere) or chained from the
# `claude-code-sdlc` installer (which curls this script and pipes to bash).
#
# Usage:
#   bash install.sh                  Install user-level binary + prompts
#   bash install.sh --yes            Skip confirmation prompts
#   bash install.sh --local          Use local checkout (skip git clone)
#   bash install.sh --help           Show help
#
# Pipe form (used by the SDLC installer):
#   curl -fsSL https://raw.githubusercontent.com/codefather-labs/claudebase/main/install.sh | bash -s -- --yes
# ============================================================================

set -u

# ============================================================================
# Constants
# ============================================================================
CLAUDEBASE_VERSION="0.9.1"
CLAUDEBASE_PDFIUM_VERSION="chromium/7802"
REPO_URL="https://github.com/codefather-labs/claudebase.git"
RELEASE_BASE="https://github.com/codefather-labs/claudebase/releases/download"

CLAUDE_DIR="$HOME/.claude"
SCRIPT_DIR=""
LOCAL_MODE=false
ASSUME_YES=false

# ============================================================================
# Logging
# ============================================================================
if [ -t 1 ]; then
  RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'
  BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'
else
  RED=''; GREEN=''; YELLOW=''; BLUE=''; BOLD=''; NC=''
fi

log_info()  { echo -e "${BLUE}[INFO]${NC} $1"; }
log_ok()    { echo -e "${GREEN}  [OK]${NC}  $1"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

print_help() {
  cat <<HELPEOF
${BOLD}claudebase installer${NC}

Installs the claudebase CLI binary and agent toolkit (rules, commands,
agents) into ~/.claude/.

USAGE:
  bash install.sh [OPTIONS]

OPTIONS:
  --yes                       Skip confirmation prompts
  --local                     Use local checkout (skip git clone)
  --help                      Show this help

WHAT GETS INSTALLED:
  ~/.claude/tools/claudebase/claudebase     CLI binary (downloaded from releases)
  ~/.claude/tools/claudebase/pdfium/        PDFium dynamic library for PDF extraction
  ~/.claude/tools/claudebase/models/        e5-multilingual-small encoder (pre-cached)
  ~/.claude/rules/cognitive-self-check.md   3-protocol discipline (Facts / Decisions / Inbound)
  ~/.claude/commands/                       /knowledge-ingest, /reflect, /consolidate, /update-claudebase
  ~/.claude/agents/                         reflection (Drift), consolidator (Mnem)
  ~/.claude/skills/                         /claudebase-daemon-change-nick,
                                            /claudebase-daemon-setup-auth-token,
                                            /claudebase-daemon-callback-info
  ~/.claude/hooks/claudebase-channel-contract.sh   SessionStart - how outside messages arrive
  ~/.claude/hooks/claudebase-read-insights-reminder.sh  SessionStart - load prior insights
  ~/.claude/hooks/claudebase-selfcheck-reminder.sh UserPromptSubmit - self-check reminder
  ~/.claude/hooks/claudebase-agent-routing-reminder.sh  PreToolUse:EnterPlanMode - peers exist
  ~/.claude/hooks/claudebase-feature-describe.sh   PostToolUse:ExitPlanMode - publish the plan
  /usr/local/bin/claudebase                 Global alias (symlink)
  ~/.claude/settings.json                   Bash allowlist + the hooks above, merged

WHAT IS *REMOVED* (reverse migration, idempotent):
  the old Stop[insight-capture] hook, the claudebase patch inside the official
  Anthropic Telegram plugin, and the retired claudebase@claudebase-dev marketplace.

AFTER INSTALLING:
  claudebase run                            start a session the daemon can reach
  claudebase telegram addbot "<token>"      wire up Telegram
  claudebase daemon callback enable         open the HTTP callback endpoint (off by default)
HELPEOF
}

# ============================================================================
# Argument parsing
# ============================================================================
while [ $# -gt 0 ]; do
  case "$1" in
    --yes|-y)    ASSUME_YES=true; shift ;;
    --local)     LOCAL_MODE=true; shift ;;
    --help|-h)   print_help; exit 0 ;;
    *) log_error "unknown flag: $1"; print_help; exit 2 ;;
  esac
done

confirm() {
  if [ "$ASSUME_YES" = true ]; then return 0; fi
  read -r -p "$1 [y/N] " ans
  case "$ans" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

# ============================================================================
# Source-dir resolution
# ============================================================================
get_source_dir() {
  if [ "$LOCAL_MODE" = true ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    if [ ! -d "$SCRIPT_DIR/prompts/rules" ] || [ ! -d "$SCRIPT_DIR/prompts/commands" ] || [ ! -d "$SCRIPT_DIR/prompts/agents" ]; then
      log_error "--local requires running from a claudebase checkout root (with prompts/{rules,commands,agents}/)"
      exit 1
    fi
  else
    SCRIPT_DIR=$(mktemp -d)
    log_info "Cloning claudebase from $REPO_URL..."
    if ! git clone --depth 1 --quiet "$REPO_URL" "$SCRIPT_DIR" 2>/dev/null; then
      log_error "Failed to clone $REPO_URL. Check your internet connection."
      rm -rf "$SCRIPT_DIR"
      exit 1
    fi
    log_ok "Repository cloned"
  fi
}

# ============================================================================
# Install prompts/rules/commands/agents into ~/.claude/
# ============================================================================
install_prompts() {
  mkdir -p "$CLAUDE_DIR/rules" "$CLAUDE_DIR/commands" "$CLAUDE_DIR/agents"

  for f in "$SCRIPT_DIR"/prompts/rules/*.md; do
    [ -f "$f" ] || continue
    cp "$f" "$CLAUDE_DIR/rules/"
    log_ok "rules/$(basename "$f")"
  done

  for f in "$SCRIPT_DIR"/prompts/commands/*.md; do
    [ -f "$f" ] || continue
    cp "$f" "$CLAUDE_DIR/commands/"
    log_ok "commands/$(basename "$f")"
  done

  for f in "$SCRIPT_DIR"/prompts/agents/*.md; do
    [ -f "$f" ] || continue
    cp "$f" "$CLAUDE_DIR/agents/"
    log_ok "agents/$(basename "$f")"
  done

  # Skills. `~/.claude/skills/<name>/SKILL.md` is a first-class skill source in
  # Claude Code, independent of plugins — which is why claudebase needs no
  # marketplace to ship one. Each skill is a DIRECTORY (the SKILL.md filename is
  # fixed), so copy per-directory rather than per-file.
  #
  # Scope note: skills here are operator conveniences that wrap a deterministic
  # CLI call. Access control deliberately does NOT ship as a skill — see the
  # v0.10 plan, Slice 10: granting access must not be a decision taken by a model
  # whose context contains messages from the channel being gated.
  if [ -d "$SCRIPT_DIR/prompts/skills" ]; then
    mkdir -p "$CLAUDE_DIR/skills"
    for d in "$SCRIPT_DIR"/prompts/skills/*/; do
      [ -d "$d" ] || continue
      name="$(basename "$d")"
      [ -f "$d/SKILL.md" ] || { log_warn "skills/$name has no SKILL.md — skipping"; continue; }
      mkdir -p "$CLAUDE_DIR/skills/$name"
      cp "$d/SKILL.md" "$CLAUDE_DIR/skills/$name/"
      log_ok "skills/$name"
    done
  fi
}

# ============================================================================
# Download claudebase binary from GitHub releases
# ============================================================================
install_binary() {
  local target_dir="$CLAUDE_DIR/tools/claudebase"
  mkdir -p "$target_dir"

  local platform exe_ext=""
  case "$(uname -ms)" in
    "Darwin arm64")  platform="darwin-arm64"  ;;
    "Darwin x86_64") platform="darwin-x64"    ;;
    "Linux x86_64")  platform="linux-x64"     ;;
    "Linux aarch64") platform="linux-arm64"   ;;
    MINGW*|MSYS*|CYGWIN*)
      case "$(uname -m)" in
        x86_64) platform="windows-x64"; exe_ext=".exe" ;;
        *)
          log_warn "unsupported Windows arch: $(uname -m); skipping binary"
          return 0
          ;;
      esac
      ;;
    *)
      log_warn "binary unavailable for $(uname -ms); install cargo or build from source"
      return 0
      ;;
  esac

  local target_bin="$target_dir/claudebase${exe_ext}"

  # --local builds the binary from THIS checkout and installs it, never
  # downloading a release asset (which may be older, different, or absent
  # after a tag was deleted). Requires a rust toolchain. On macOS the
  # freshly-copied Mach-O is re-signed ad-hoc so Gatekeeper does not
  # SIGKILL it on first exec: a plain cp of a signed binary invalidates
  # the signature and the kernel kills the process with code 137.
  if [ "$LOCAL_MODE" = true ]; then
    if ! command -v cargo >/dev/null 2>&1; then
      log_error "--local binary build needs cargo (install the rust toolchain via rustup)"
      return 1
    fi
    ensure_build_deps || return 1
    log_info "building claudebase from local checkout ($SCRIPT_DIR) via cargo build --release"
    if ! ( cd "$SCRIPT_DIR" && cargo build --release --features asr-whisper ); then
      log_error "local 'cargo build --release' failed; binary not installed"
      return 1
    fi
    local local_bin="$SCRIPT_DIR/target/release/claudebase${exe_ext}"
    if [ ! -x "$local_bin" ]; then
      log_error "local build produced no binary at $local_bin"
      return 1
    fi
    cp "$local_bin" "$target_bin"
    chmod +x "$target_bin"
    if [ "$(uname -s)" = "Darwin" ] && command -v codesign >/dev/null 2>&1; then
      if codesign --force --sign - "$target_bin" >/dev/null 2>&1; then
        log_ok "re-signed local binary (macOS Gatekeeper)"
      else
        log_warn "codesign failed; if the binary is Killed:9 on exec, run: codesign --force --sign - $target_bin"
      fi
    fi
    log_ok "tools/claudebase/claudebase (local build, $platform)"
    return 0
  fi

  if [ -x "$target_bin" ]; then
    local existing_ver
    existing_ver="$("$target_bin" --version 2>/dev/null | awk '{print $2}' || true)"
    if [ "$existing_ver" = "$CLAUDEBASE_VERSION" ]; then
      log_ok "claudebase binary already at version $CLAUDEBASE_VERSION"
      return 0
    fi
  fi

  local url="${RELEASE_BASE}/claudebase-v${CLAUDEBASE_VERSION}/claudebase-${platform}${exe_ext}"
  local tmp; tmp="$(mktemp)"

  if command -v curl >/dev/null 2>&1; then
    if ! curl --proto '=https' --tlsv1.2 -fsSL --max-redirs 5 --max-time 120 "$url" -o "$tmp"; then
      rm -f "$tmp"
      log_warn "claudebase binary download failed (curl). Build from source: cargo install --git $REPO_URL"
      return 0
    fi
  elif command -v wget >/dev/null 2>&1; then
    if ! wget --https-only --secure-protocol=TLSv1_2 --max-redirect=5 --timeout=120 -q -O "$tmp" "$url"; then
      rm -f "$tmp"
      log_warn "claudebase binary download failed (wget). Build from source: cargo install --git $REPO_URL"
      return 0
    fi
  else
    rm -f "$tmp"
    log_warn "neither curl nor wget available; cannot install binary"
    return 0
  fi

  chmod +x "$tmp"
  if ! "$tmp" --version >/dev/null 2>&1; then
    log_warn "downloaded binary failed --version smoke; not installing"
    rm -f "$tmp"
    return 0
  fi

  mv "$tmp" "$target_bin"
  chmod +x "$target_bin"
  log_ok "tools/claudebase/claudebase ($platform)"
}

# ============================================================================
# Register global `claudebase` alias (symlink into first writable PATH dir)
# ============================================================================
register_alias() {
  local exe_ext=""
  case "$(uname -ms)" in MINGW*|MSYS*|CYGWIN*) exe_ext=".exe" ;; esac
  local target_bin="$CLAUDE_DIR/tools/claudebase/claudebase${exe_ext}"

  if [ ! -x "$target_bin" ]; then
    log_warn "alias: target binary not found at $target_bin; skipping"
    return 0
  fi

  local link_dir=""
  for dir in "/usr/local/bin" "/opt/homebrew/bin" "$HOME/.local/bin"; do
    if [ -d "$dir" ] && [ -w "$dir" ]; then link_dir="$dir"; break; fi
  done
  if [ -z "$link_dir" ]; then
    if mkdir -p "$HOME/.local/bin" 2>/dev/null && [ -w "$HOME/.local/bin" ]; then
      link_dir="$HOME/.local/bin"
    fi
  fi
  if [ -z "$link_dir" ]; then
    log_warn "alias: no writable PATH directory found"
    log_warn "  manual setup: ln -sf $target_bin /usr/local/bin/claudebase"
    return 0
  fi

  local link_path="$link_dir/claudebase"
  if [ -e "$link_path" ] && [ ! -L "$link_path" ]; then
    log_warn "alias: $link_path is a regular file; refusing to overwrite"
    return 0
  fi
  if [ -L "$link_path" ] && [ "$(readlink "$link_path")" = "$target_bin" ]; then
    log_ok "claudebase alias already in place ($link_path)"
    return 0
  fi
  rm -f "$link_path"
  ln -s "$target_bin" "$link_path"
  log_ok "claudebase alias: $link_path -> $target_bin"

  case ":$PATH:" in
    *":$link_dir:"*) ;;
    *)
      log_warn "  NOTE: $link_dir is not on PATH for the current shell"
      log_warn "  add to your shell rc: export PATH=\"$link_dir:\$PATH\""
      ;;
  esac
}

# ============================================================================
# Bash allowlist merge (settings.json)
# ============================================================================
register_bash_allowlist() {
  local settings="$CLAUDE_DIR/settings.json"
  # Two forms on purpose: the absolute path (how hooks and older prompts call
  # it) and the bare name (how an agent calls it after `register_alias` put a
  # symlink on PATH — `claudebase telegram send ...`). Without the bare form
  # every reply to a Telegram message would raise a permission prompt.
  local entry='~/.claude/tools/claudebase/claudebase *'
  local entry_bare='claudebase *'

  if [ ! -f "$settings" ]; then
    mkdir -p "$CLAUDE_DIR"
    cat > "$settings" <<EOF
{"permissions":{"allow":["$entry","$entry_bare"]}}
EOF
    chmod 0644 "$settings"
    log_ok "settings.json (created with claudebase allowlist)"
    return 0
  fi

  if command -v jq >/dev/null 2>&1; then
    local tmp; tmp="$(mktemp)"
    if jq --arg new "$entry" --arg bare "$entry_bare" \
         '(.permissions //= {}) | (.permissions.allow //= []) | .permissions.allow = ((.permissions.allow + [$new, $bare]) | unique)' \
         "$settings" > "$tmp" \
       && jq -e '.' "$tmp" >/dev/null 2>&1; then
      mv "$tmp" "$settings"
      chmod 0644 "$settings"
      log_ok "settings.json (claudebase allowlist merged)"
    else
      rm -f "$tmp"
      log_warn "settings.json merge failed; add manually: $entry"
    fi
  else
    if grep -Fq "$entry" "$settings"; then
      log_ok "settings.json already contains claudebase allowlist"
    else
      log_warn "jq required for safe settings.json merge — install jq or add manually: $entry"
    fi
  fi
}

# ============================================================================
# Install claudebase hooks into ~/.claude/hooks/ and wire them into
# ~/.claude/settings.json. Five hooks across two layers — the cognitive-infra
# layer and the transport layer:
#
#   - SessionStart -> claudebase-channel-contract.sh — teaches the session how
#     messages from outside the terminal arrive ([telegram_message]:,
#     [agent-to-agent:<nick>]:, [callback]:) and that a reply only leaves
#     through a CLI call. It has to be in context BEFORE the first such line
#     appears, which is why it is a hook and not a skill.
#   - SessionStart -> claudebase-read-insights-reminder.sh — load prior-session
#     insights by tag rather than re-reading everything.
#   - PreToolUse:EnterPlanMode -> claudebase-agent-routing-reminder.sh — surface
#     that peers exist before drafting a plan that collides with theirs.
#   - PostToolUse:ExitPlanMode -> claudebase-feature-describe.sh — publish what
#     was decided so peers can see it.
#   - UserPromptSubmit -> claudebase-selfcheck-reminder.sh — fires before the
#     agent responds, injects a SHORT agent-only reminder of the three
#     cognitive-self-check protocols (the rule it reminds about,
#     cognitive-self-check.md, ships from claudebase too).
#
# Idempotent — jq merge is by command-string equality, so re-running never
# duplicates an entry.
# ============================================================================
install_claudebase_hooks() {
  local hooks_dir="$CLAUDE_DIR/hooks"
  local settings="$CLAUDE_DIR/settings.json"

  mkdir -p "$hooks_dir"

  # Remove the retired Stop insight-capture hook files (superseded by the
  # UserPromptSubmit reminder).
  rm -f "$hooks_dir/claudebase-insight-capture.sh" "$hooks_dir/claudebase-insight-capture.ps1"

  local hook_files=(claudebase-selfcheck-reminder.sh claudebase-selfcheck-reminder.ps1 claudebase-read-insights-reminder.sh claudebase-read-insights-reminder.ps1 claudebase-agent-routing-reminder.sh claudebase-agent-routing-reminder.ps1 claudebase-feature-describe.sh claudebase-feature-describe.ps1 claudebase-channel-contract.sh claudebase-channel-contract.ps1)
  for hook in "${hook_files[@]}"; do
    local src="$SCRIPT_DIR/hooks/$hook"
    local dst="$hooks_dir/$hook"
    if [ ! -f "$src" ]; then
      log_warn "hooks/$hook missing in source — skipping"
      continue
    fi
    cp "$src" "$dst"
    chmod 0755 "$dst"
    log_ok "hooks/$hook"
  done

  if [ ! -f "$settings" ]; then
    mkdir -p "$CLAUDE_DIR"
    echo '{"permissions":{"allow":[]}}' > "$settings"
    chmod 0644 "$settings"
  fi

  if ! command -v jq >/dev/null 2>&1; then
    log_warn "jq required for settings.json hook merge — add manually:"
    log_warn '  hooks.UserPromptSubmit[*].hooks[*].command = ~/.claude/hooks/claudebase-selfcheck-reminder.sh'
    log_warn '  hooks.SessionStart[*].hooks[*].command = ~/.claude/hooks/claudebase-read-insights-reminder.sh'
    log_warn '  hooks.SessionStart[*].hooks[*].command = ~/.claude/hooks/claudebase-channel-contract.sh'
    log_warn '  (and remove any hooks.Stop entry pointing at claudebase-insight-capture.sh)'
    return 0
  fi

  local stop_cmd="$HOME/.claude/hooks/claudebase-insight-capture.sh"
  local selfcheck_cmd="$HOME/.claude/hooks/claudebase-selfcheck-reminder.sh"
  local readins_cmd="$HOME/.claude/hooks/claudebase-read-insights-reminder.sh"
  local routing_cmd="$HOME/.claude/hooks/claudebase-agent-routing-reminder.sh"
  local describe_cmd="$HOME/.claude/hooks/claudebase-feature-describe.sh"
  # The transport contract. Without it a session receives `[telegram_message]:`
  # lines it was never told about, and answers into a terminal the sender
  # cannot see -- the reply only leaves the machine through a CLI call.
  local channel_cmd="$HOME/.claude/hooks/claudebase-channel-contract.sh"
  local tmp; tmp="$(mktemp)"

  # (1) Ensure .hooks.UserPromptSubmit has exactly one matcher block with our
  #     command. (2) Actively UNWIRE the retired Stop insight-capture hook:
  #     drop matcher blocks whose only command was claudebase-insight-capture,
  #     and remove that command from any shared block. Foreign matchers stay.
  # (3) Idempotently wire the SessionStart read-insights reminder. Match by
  #     command-string equality across ALL SessionStart blocks (foreign blocks
  #     and the SDLC onboarding block are preserved). The official SessionStart
  #     shape nests command under a matcher block: {matcher, hooks[{type,command}]}.
  if jq \
      --arg stop_cmd "$stop_cmd" \
      --arg selfcheck_cmd "$selfcheck_cmd" \
      --arg readins_cmd "$readins_cmd" \
      --arg routing_cmd "$routing_cmd" \
      --arg describe_cmd "$describe_cmd" \
      --arg channel_cmd "$channel_cmd" \
      '
      .hooks //= {}
      | .hooks.Stop //= []
      | .hooks.UserPromptSubmit //= []
      | .hooks.Stop |=
          (if any(.[]?; (.hooks // []) | any(.command == $stop_cmd))
           then .
           else . + [{"hooks": [{"type": "command", "command": $stop_cmd}]}]
           end)
      | .hooks.UserPromptSubmit |=
          (if any(.[]?; (.hooks // []) | any(.command == $selfcheck_cmd))
           then .
           else . + [{"hooks": [{"type": "command", "command": $selfcheck_cmd}]}]
           end)
      | .hooks.SessionStart //= []
      | .hooks.SessionStart |=
          (if any(.[]?; (.hooks // []) | any(.command == $readins_cmd))
           then .
           else . + [{"matcher": "startup|resume|compact", "hooks": [{"type": "command", "command": $readins_cmd}]}]
           end)
      | .hooks.SessionStart |=
          (if any(.[]?; (.hooks // []) | any(.command == $channel_cmd))
           then .
           else . + [{"matcher": "startup|resume|compact", "hooks": [{"type": "command", "command": $channel_cmd}]}]
           end)
      | .hooks.PreToolUse //= []
      | .hooks.PreToolUse |=
          (if any(.[]?; (.hooks // []) | any(.command == $routing_cmd))
           then .
           else . + [{"matcher": "EnterPlanMode", "hooks": [{"type": "command", "command": $routing_cmd}]}]
           end)
      | .hooks.PostToolUse //= []
      | .hooks.PostToolUse |=
          (if any(.[]?; (.hooks // []) | any(.command == $describe_cmd))
           then .
           else . + [{"matcher": "ExitPlanMode", "hooks": [{"type": "command", "command": $describe_cmd}]}]
           end)
      | (if (.hooks.Stop // []) | length > 0 then
           .hooks.Stop |= (
             map(.hooks |= (map(select(.command != $stop_cmd))))
             | map(select((.hooks // []) | length > 0))
           )
         else . end)
      | (if (.hooks.Stop // []) | length == 0 then del(.hooks.Stop) else . end)
      ' \
      "$settings" > "$tmp" 2>/dev/null \
     && jq -e . "$tmp" >/dev/null 2>&1; then
    mv "$tmp" "$settings"
    chmod 0644 "$settings"
    log_ok "settings.json (UserPromptSubmit[selfcheck] + SessionStart[read-insights,channel-contract] wired; retired Stop[insight-capture] unwired)"
  else
    rm -f "$tmp"
    log_warn "settings.json hook merge failed; please add manually"
  fi
}

# ============================================================================
# Build dependencies — ONLY for --local
# ============================================================================
# The default install path downloads a pre-built binary and compiles nothing,
# so it needs none of this. Only `--local` compiles the crate.
#
# OpenSSL is deliberately absent from this list. It used to be required —
# `fastembed -> hf-hub -> native-tls` and `teloxide-core -> reqwest ->
# native-tls` pulled `openssl-sys`, which needs the development headers, and the
# resulting binary linked `libssl.so.3` at RUNTIME so a release build refused to
# start on any distribution carrying a different OpenSSL major. Both crates were
# moved to rustls, `openssl-sys` left the dependency tree entirely, and a clean
# Ubuntu 24.04 container built and installed successfully with no `libssl-dev`
# present. If a future dependency reintroduces it, the install-smoke workflow
# fails on the `ldd` assertion before anyone has to debug it.
#
# Deliberately does NOT install anything without consent: this is the only
# place the installer would touch system packages, and a script that silently
# runs `sudo apt-get install` is a script nobody should pipe from the internet.
# Missing pieces are named, the exact command is printed, and it runs only if
# the operator agreed (--yes, or the confirmation prompt).
ensure_build_deps() {
  [ "$LOCAL_MODE" = true ] || return 0

  local missing_desc=()
  command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || missing_desc+=("a C compiler")
  command -v pkg-config >/dev/null 2>&1 || missing_desc+=("pkg-config")

  # The local build runs with --features asr-whisper, which compiles
  # whisper.cpp through whisper-rs-sys: cmake drives the build and bindgen
  # needs clang's own resource headers. Observed on a clean Ubuntu 24.04 on
  # 2026-08-17: without them the build dies far from its cause, with
  # `fatal error: 'stdbool.h' file not found` inside ggml.h -- which names
  # neither cmake, nor clang, nor the feature that pulled either in.
  command -v cmake >/dev/null 2>&1 || missing_desc+=("cmake (whisper build)")
  command -v clang >/dev/null 2>&1 || missing_desc+=("clang + libclang (bindgen)")

  [ ${#missing_desc[@]} -eq 0 ] && return 0

  local pm="" cmd=""
  if   command -v apt-get >/dev/null 2>&1; then pm=apt-get; cmd="sudo apt-get install -y build-essential pkg-config cmake clang libclang-dev"
  elif command -v dnf     >/dev/null 2>&1; then pm=dnf;     cmd="sudo dnf install -y gcc pkgconf-pkg-config cmake clang clang-devel"
  elif command -v yum     >/dev/null 2>&1; then pm=yum;     cmd="sudo yum install -y gcc pkgconfig cmake clang clang-devel"
  elif command -v pacman  >/dev/null 2>&1; then pm=pacman;  cmd="sudo pacman -S --needed --noconfirm base-devel pkgconf cmake clang"
  elif command -v zypper  >/dev/null 2>&1; then pm=zypper;  cmd="sudo zypper install -y gcc pkg-config cmake clang llvm-clang-devel"
  elif command -v apk     >/dev/null 2>&1; then pm=apk;     cmd="sudo apk add build-base pkgconfig cmake clang clang-dev"
  elif command -v brew    >/dev/null 2>&1; then pm=brew;    cmd="brew install pkg-config cmake llvm"
  fi

  log_warn "the local build needs: ${missing_desc[*]}"

  if [ -z "$pm" ]; then
    log_error "no supported package manager found — install the above, then re-run with --local"
    return 1
  fi

  log_info "install command for this system:"
  log_info "  $cmd"

  if [ "$ASSUME_YES" != true ] && ! confirm "Run it now?"; then
    log_error "build dependencies not installed; run the command above and retry"
    return 1
  fi

  if ! eval "$cmd"; then
    log_error "installing build dependencies failed; run the command manually and retry"
    return 1
  fi

  log_ok "build dependencies present"
}

# ============================================================================
# Install pdfium native library
# ============================================================================
install_pdfium() {
  (
    set +e
    umask 0022

    local target_dir="$CLAUDE_DIR/tools/claudebase/pdfium"
    local lib_dir="$target_dir/lib"
    local sentinel="$target_dir/.version"

    if [ -f "$sentinel" ]; then
      local existing; existing=$(cat "$sentinel" 2>/dev/null)
      if [ "$existing" = "$CLAUDEBASE_PDFIUM_VERSION" ]; then
        log_ok "pdfium already at version $CLAUDEBASE_PDFIUM_VERSION"
        return 0
      fi
    fi

    local platform asset
    case "$(uname -s)/$(uname -m)" in
      Darwin/arm64)   platform=darwin-arm64;  asset=pdfium-mac-arm64.tgz   ;;
      Darwin/x86_64)  platform=darwin-x64;    asset=pdfium-mac-x64.tgz     ;;
      Linux/x86_64)   platform=linux-x64;     asset=pdfium-linux-x64.tgz   ;;
      Linux/aarch64)  platform=linux-arm64;   asset=pdfium-linux-arm64.tgz ;;
      *)
        log_warn "pdfium unavailable for $(uname -s)/$(uname -m); PDF extraction will fail at runtime"
        return 0
        ;;
    esac

    local url="https://github.com/bblanchon/pdfium-binaries/releases/download/${CLAUDEBASE_PDFIUM_VERSION}/${asset}"
    local tmp_archive staging
    tmp_archive=$(mktemp -t pdfium.XXXXXX) || { log_warn "mktemp failed"; return 0; }
    staging=$(mktemp -d -t pdfium.XXXXXX) || { log_warn "mktemp -d failed"; rm -f "$tmp_archive"; return 0; }
    trap 'rm -f "$tmp_archive"; rm -rf "$staging" 2>/dev/null' EXIT

    if command -v curl >/dev/null 2>&1; then
      if ! curl --proto '=https' --tlsv1.2 -fsSL --max-redirs 5 --max-time 120 "$url" -o "$tmp_archive"; then
        log_warn "pdfium download failed (curl); skipping PDF support"; return 0
      fi
    elif command -v wget >/dev/null 2>&1; then
      if ! wget --https-only --secure-protocol=TLSv1_2 --max-redirect=5 --timeout=120 -q -O "$tmp_archive" "$url"; then
        log_warn "pdfium download failed (wget); skipping PDF support"; return 0
      fi
    else
      log_warn "neither curl nor wget available; skipping pdfium"; return 0
    fi

    if tar -tzf "$tmp_archive" 2>/dev/null | grep -E '^/|(^|/)\.\.(/|$)' >/dev/null; then
      log_warn "pdfium archive contains traversal entries; refusing"; return 0
    fi

    if ! tar --no-same-owner --no-same-permissions -xzf "$tmp_archive" -C "$staging" 2>/dev/null; then
      log_warn "pdfium extraction failed"; return 0
    fi

    if find "$staging" -path '*..*' -print -quit 2>/dev/null | grep -q .; then
      log_warn "pdfium produced traversal paths post-extract; refusing"; return 0
    fi

    if find "$staging" -perm /6000 -print -quit 2>/dev/null | grep -q .; then
      log_warn "pdfium contains setuid/setgid files; refusing"; return 0
    fi

    local extracted_lib
    extracted_lib=$(find "$staging" -maxdepth 3 -name "libpdfium*" -type f -print -quit 2>/dev/null)
    if [ -z "$extracted_lib" ]; then
      log_warn "no libpdfium found in extracted archive"; return 0
    fi

    mkdir -p "$lib_dir"
    cp "$extracted_lib" "$lib_dir/"
    chmod 0755 "$lib_dir"/libpdfium*
    echo "$CLAUDEBASE_PDFIUM_VERSION" > "$sentinel"
    chmod 0644 "$sentinel"

    if ! [ -s "$lib_dir/libpdfium.dylib" ] && ! [ -s "$lib_dir/libpdfium.so" ]; then
      log_warn "pdfium post-install integrity check failed"
      rm -rf "$target_dir"
      return 0
    fi

    log_ok "pdfium installed: ${platform} (version ${CLAUDEBASE_PDFIUM_VERSION})"
    return 0
  )
  return 0
}

# ============================================================================
# Undo the old plugin-slot hijack (v0.10 reverse migration)
# ============================================================================
# Until v0.10 the installer wrote our own binary into the OFFICIAL Anthropic
# Telegram plugin's cache and rewrote its `.mcp.json` so the plugin's MCP
# server was `claudebase plugin serve`. That transport is gone: Telegram lives
# inside the daemon and messages reach Claude Code through the PTY supervisor
# (`claudebase run`).
#
# The patch must be actively UNDONE, not merely stopped. An operator who
# upgrades would otherwise keep a third-party plugin permanently pointing at
# our binary — mess we created, so we clean it:
#
#   * `.mcp.json` is restored from the `.mcp.json.upstream-backup` saved at
#     patch time. With no backup the file is left alone and the operator is
#     told to reinstall the plugin — guessing upstream's command line would be
#     worse than saying so plainly.
#   * `server-rs` (our binary) is deleted from the plugin's cache directory.
#
# Also drops the even older `claudebase@claudebase-dev` marketplace + plugin
# registration from the v0.6 era. Idempotent: every step no-ops when its
# artefact is already absent.
unpatch_official_telegram_plugin() {
  if ! command -v claude >/dev/null 2>&1; then
    return 0
  fi

  # v0.6-era leftovers: uninstall before removing the marketplace, otherwise
  # the removal is rejected with 'plugin still installed from this source'.
  claude plugin uninstall claudebase@claudebase-dev >/dev/null 2>&1 || true
  claude plugin marketplace remove codefather-labs/claudebase >/dev/null 2>&1 || true

  local plugin_root="$CLAUDE_DIR/plugins/cache/claude-plugins-official/telegram"
  if [ ! -d "$plugin_root" ]; then
    log_ok "no official telegram plugin cache — nothing to unpatch"
    return 0
  fi

  local restored=0 removed=0 orphaned=0
  local version_dir
  for version_dir in "$plugin_root"/*/; do
    [ -d "$version_dir" ] || continue
    if [ -f "${version_dir}.mcp.json.upstream-backup" ]; then
      mv -f "${version_dir}.mcp.json.upstream-backup" "${version_dir}.mcp.json"
      restored=$((restored + 1))
    elif grep -q "claudebase" "${version_dir}.mcp.json" 2>/dev/null; then
      orphaned=$((orphaned + 1))
    fi
    if [ -f "${version_dir}server-rs" ] || [ -f "${version_dir}server-rs.exe" ]; then
      rm -f "${version_dir}server-rs" "${version_dir}server-rs.exe"
      removed=$((removed + 1))
    fi
  done

  if [ "$restored" -gt 0 ] || [ "$removed" -gt 0 ]; then
    log_ok "official telegram plugin unpatched (.mcp.json restored: $restored, server-rs removed: $removed)"
  else
    log_ok "official telegram plugin carries no claudebase patch (idempotent)"
  fi
  if [ "$orphaned" -gt 0 ]; then
    log_warn "$orphaned plugin dir(s) still point at claudebase with no upstream backup"
    log_warn "  restore upstream with: claude plugin install telegram@claude-plugins-official"
  fi
}
# ============================================================================
# Pre-warm e5 encoder so first `claudebase ingest` doesn't pay ~30s cold start
# ============================================================================
preload_encoder() {
  local exe_ext=""
  case "$(uname -ms)" in MINGW*|MSYS*|CYGWIN*) exe_ext=".exe" ;; esac
  local bin="$CLAUDE_DIR/tools/claudebase/claudebase${exe_ext}"
  if [ ! -x "$bin" ]; then return 0; fi

  log_info "Pre-loading e5-multilingual-small encoder (~120 MB on first run)..."
  if "$bin" warmup --quiet 2>&1; then
    log_ok "encoder ready (cached at ~/.claude/tools/claudebase/models/)"
  else
    log_warn "encoder pre-load failed; fastembed will retry on first ingest"
  fi
}

# ============================================================================
# Install whisper-cli + ffmpeg (voice transcription dependencies)
# ============================================================================
# Needed by the upcoming Rust port of the official Telegram plugin which
# transcribes inbound voice messages locally via whisper.cpp before
# emitting them as channel notifications.
#
# Best-effort:
#   - If both binaries are already on PATH → log + return 0 (idempotent).
#   - If a package manager is detected → attempt install; warn on failure.
#   - If no package manager → log clear manual-install hint + return 0.
# The actual whisper model (~1.5 GB ggml-medium.bin) is NOT downloaded
# here — too heavy for the install path. The plugin downloads it lazily
# on first voice message, or the operator drops it at
# ~/.local/share/whisper-cpp/models/ggml-medium.bin ahead of time.
#
# Opt-out: set CLAUDEBASE_SKIP_WHISPER=1 to bypass entirely (no install,
# no log spam). For headless CI where audio deps would just add minutes
# to the install for no benefit.
install_whisper_stack() {
  if [ "${CLAUDEBASE_SKIP_WHISPER:-0}" = "1" ]; then
    log_info "CLAUDEBASE_SKIP_WHISPER=1 — skipping ffmpeg + whisper-cli install"
    return 0
  fi

  local need_ffmpeg=true
  local need_whisper=true
  command -v ffmpeg >/dev/null 2>&1 && need_ffmpeg=false
  command -v whisper-cli >/dev/null 2>&1 && need_whisper=false

  if ! $need_ffmpeg && ! $need_whisper; then
    log_ok "ffmpeg + whisper-cli already on PATH (voice transcription ready)"
    return 0
  fi

  # Detect package manager (try most reliable first).
  local pkg_mgr=""
  local pkg_install=""
  local pkg_ffmpeg="ffmpeg"
  local pkg_whisper="whisper-cpp"
  if command -v brew >/dev/null 2>&1; then
    pkg_mgr="brew"
    pkg_install="brew install"
  elif command -v apt-get >/dev/null 2>&1; then
    pkg_mgr="apt-get"
    pkg_install="sudo apt-get install -y"
  elif command -v dnf >/dev/null 2>&1; then
    pkg_mgr="dnf"
    pkg_install="sudo dnf install -y"
  elif command -v pacman >/dev/null 2>&1; then
    pkg_mgr="pacman"
    pkg_install="sudo pacman -S --noconfirm"
  else
    log_warn "no supported package manager detected (brew/apt-get/dnf/pacman); voice transcription disabled"
    log_warn "  to enable, install manually:"
    log_warn "    macOS:  brew install whisper-cpp ffmpeg"
    log_warn "    Linux:  apt install whisper-cpp ffmpeg  (or dnf/pacman equivalent)"
    return 0
  fi

  if $need_ffmpeg; then
    log_info "installing ffmpeg via $pkg_mgr..."
    if $pkg_install $pkg_ffmpeg >/dev/null 2>&1; then
      log_ok "ffmpeg installed"
    else
      log_warn "ffmpeg install via $pkg_mgr failed; install manually: $pkg_install $pkg_ffmpeg"
    fi
  fi

  if $need_whisper; then
    log_info "installing whisper-cli via $pkg_mgr (this can take a few minutes)..."
    if $pkg_install $pkg_whisper >/dev/null 2>&1; then
      log_ok "whisper-cli installed"
    else
      log_warn "whisper-cli install via $pkg_mgr failed; install manually: $pkg_install $pkg_whisper"
    fi
  fi

  if command -v ffmpeg >/dev/null 2>&1 && command -v whisper-cli >/dev/null 2>&1; then
    log_info "voice transcription stack ready — whisper model auto-downloads on first voice msg"
    log_info "  (or pre-download to ~/.local/share/whisper-cpp/models/ggml-medium.bin)"
  fi
  return 0
}

# ============================================================================
# Main
# ============================================================================
echo ""
echo -e "${BOLD}============================================${NC}"
echo -e "${BOLD}  claudebase v${CLAUDEBASE_VERSION} — installer${NC}"
echo -e "${BOLD}============================================${NC}"
echo ""
echo "  This will install to $CLAUDE_DIR:"
echo "    tools/claudebase/   (binary + pdfium + e5 model)"
echo "    rules/              (1 file - cognitive-self-check)"
echo "    commands/           (4 files — knowledge-ingest, reflect, consolidate, update-claudebase)"
echo "    agents/             (2 files — reflection, consolidator)"
echo "    skills/             (3 skills — change-nick, setup-auth-token, callback-info)"
echo "    hooks/              (5 hooks — 2x SessionStart, UserPromptSubmit, Pre/PostToolUse)"
echo ""

if ! confirm "Proceed with installation?"; then
  log_info "Aborted."
  exit 0
fi

get_source_dir
install_prompts
# Defined since the hooks landed, but never invoked until 2026-08-17 — found by
# installing onto a clean machine, where ~/.claude/hooks simply did not exist.
# Without it a session receives `[telegram_message]:` / `[callback]:` lines it
# was never told about, which is the one thing the transport cannot recover from
# on its own.
install_claudebase_hooks
install_binary
register_alias
register_bash_allowlist
install_pdfium
install_whisper_stack
preload_encoder
unpatch_official_telegram_plugin

# ============================================================================
# Post-install daemon-as-service (default-on, idempotent full lifecycle)
# ============================================================================
# Always-on by default: replace any existing daemon service unit with the
# fresh one and start it. Opt-out via `CLAUDEBASE_SKIP_DAEMON=1`.
#
# launchd (macOS) and systemd-user (Linux) both run user-scope — no root
# required. The 'stop || true / uninstall || true' prelude makes the
# whole block idempotent even on a fresh box where there's no existing
# service to stop or uninstall.
#
# Linux: systemd user units only auto-start when the user is logged in
# OR `loginctl enable-linger $USER` has been run. We surface that hint
# but do not auto-enable linger (would change system PAM state).
if [ "${CLAUDEBASE_SKIP_DAEMON:-0}" = "1" ]; then
  log_info "CLAUDEBASE_SKIP_DAEMON=1 — skipping daemon service install"
else
  log_info "Installing claudebase daemon as user service (idempotent)..."
  # Stop and uninstall any prior service before reinstalling the plist.
  # CRITICAL: --keep-data. A bare `daemon uninstall --yes` FULL-WIPES the
  # operator's bot token (secrets.toml), daemon config (daemon.toml) AND
  # chat history (chat.db) — running that inside the installer means every
  # upgrade silently destroys the user's setup. An install/upgrade MUST be
  # non-destructive, so we keep data and only swap the service definition.
  # Silenced + '|| true' so a clean box with no prior service still works.
  claudebase daemon stop >/dev/null 2>&1 || true
  claudebase daemon uninstall --yes --keep-data >/dev/null 2>&1 || true
  if claudebase daemon install --yes >/dev/null 2>&1; then
    if claudebase daemon start; then
      log_ok "Daemon installed and started (auto-starts on next user session)"
      if [ "$(uname -s)" = "Linux" ]; then
        log_info "  Tip (Linux): for boot-time start without login, run once:"
        log_info "    loginctl enable-linger \"$USER\""
      fi
    else
      log_warn "Daemon installed but failed to start. Try 'claudebase daemon start' manually."
    fi
  else
    log_warn "Daemon install failed; continuing without daemon."
    log_warn "  Re-run later: claudebase daemon install && claudebase daemon start"
  fi
fi

# Cleanup the temp clone (only when we made one).
if [ "$LOCAL_MODE" = false ] && [ -n "$SCRIPT_DIR" ] && [ -d "$SCRIPT_DIR" ] && [ "$SCRIPT_DIR" != "/" ]; then
  rm -rf "$SCRIPT_DIR"
fi

echo ""
echo -e "${BOLD}============================================${NC}"
echo -e "${BOLD}  claudebase install complete${NC}"
echo -e "${BOLD}============================================${NC}"
echo ""
echo "  Quick start:"
echo "    claudebase --version                  Confirm binary is on PATH"
echo "    claudebase ingest <path>              Ingest PDF/MD/TXT into <cwd>/.claude/knowledge/"
echo "    claudebase search '<query>' --json    Hybrid retrieval over the books corpus"
echo "    claudebase insight create '...' \\     Persist a cognitive insight (insights corpus)"
echo "        --type agent-learned --agent <name>"
echo "    claudebase compare '<query>'          A/B-test all 3 retrieval modes"
echo ""
echo "  Commands installed (~/.claude/commands/):"
echo "    /knowledge-ingest    Ingest a folder/file into the per-project knowledge base"
echo "    /reflect             DMN unfocused observation pass — user-invoked"
echo "    /consolidate         Cross-artifact drift detection (auto-chained between waves)"
echo "    /update-claudebase   Update this installation to the latest release"
echo ""
echo "  Skills installed (~/.claude/skills/):"
echo "    /claudebase-daemon-change-nick        Rename this session so /switch can address it"
echo "    /claudebase-daemon-callback-info      How something outside pings this session"
echo "    /claudebase-daemon-setup-auth-token   The HTTP callback token for a session"
echo ""
echo "  Agents installed:"
echo "    reflection (Drift)       Default Mode Network observation pass"
echo "    consolidator (Mnem)      Hippocampal-replay drift detection"
echo ""
echo "  Tip: re-ingest existing PDFs (\`claudebase ingest <path>\`) to upgrade"
echo "  pre-v2 indexes to schema v3 — that's what unlocks per-page citations."
echo ""
