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
CLAUDEBASE_PDFIUM_VERSION="chromium/7802"
REPO_URL="https://github.com/codefather-labs/claudebase.git"
RELEASE_BASE="https://github.com/codefather-labs/claudebase/releases/download"

# Fallback version used only when the remote tag lookup below fails
# (air-gapped machine, GitHub unreachable, etc). NOT authoritative —
# the actual version installed is whatever `detect_claudebase_version`
# resolves at runtime. Bump on each release as a courtesy for cold-start
# installs without network, but absence of bump no longer breaks anything.
CLAUDEBASE_FALLBACK_VERSION="0.8.0"

# Authoritative version resolution (v0.7.1+): authoritative source is
# the latest `claudebase-v*` tag on origin, fetched via `git ls-remote`.
# Eliminates the chronic "bump install.sh manually on every release"
# trap that caused v0.7.0 to silently no-op for upgrading users.
#
# Priority order:
#   1. Operator override: CLAUDEBASE_VERSION=0.7.0 bash install.sh
#      (for pinning / downgrade / repeatable CI installs).
#   2. Latest claudebase-v* tag from origin via `git ls-remote`
#      (no GitHub API rate limit, no jq dep, semver-sorted via sort -V).
#   3. CLAUDEBASE_FALLBACK_VERSION baked above (offline / GH down).
detect_claudebase_version() {
  if [ -n "${CLAUDEBASE_VERSION:-}" ]; then
    echo "$CLAUDEBASE_VERSION"
    return 0
  fi
  if command -v git >/dev/null 2>&1; then
    local latest
    latest=$(git ls-remote --tags --refs "$REPO_URL" 'refs/tags/claudebase-v*' 2>/dev/null \
      | awk -F/ '{print $NF}' \
      | sed 's/^claudebase-v//' \
      | sort -V \
      | tail -1)
    if [ -n "$latest" ]; then
      echo "$latest"
      return 0
    fi
  fi
  echo "$CLAUDEBASE_FALLBACK_VERSION"
}
CLAUDEBASE_VERSION="$(detect_claudebase_version)"

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
  ~/.claude/rules/knowledge-base.md         CLI contract + citation discipline
  ~/.claude/rules/knowledge-base-tool.md    Usage mandate + insights protocol
  ~/.claude/rules/tool-limitations.md       Read/grep/bash truncation gotchas
  ~/.claude/hooks/claudebase-selfcheck-reminder.sh UserPromptSubmit hook — self-check protocols + insight-capture
  ~/.claude/commands/knowledge-ingest.md    /knowledge-ingest skill
  ~/.claude/commands/reflect.md             /reflect skill (DMN observation)
  ~/.claude/commands/consolidate.md         /consolidate skill (drift detection)
  ~/.claude/agents/reflection.md            reflection agent (Drift persona)
  ~/.claude/agents/consolidator.md          consolidator agent (Mnem persona)
  /usr/local/bin/claudebase                 Global alias (symlink)
  ~/.claude/settings.json                   Bash allowlist entry merged
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
# Install prompts/{rules,commands,agents}/ into ~/.claude/{rules,commands,agents}/
# (source layout: prompts/ dir at repo root; install destination: global ~/.claude/)
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
  local entry='~/.claude/tools/claudebase/claudebase *'

  if [ ! -f "$settings" ]; then
    mkdir -p "$CLAUDE_DIR"
    cat > "$settings" <<EOF
{"permissions":{"allow":["$entry"]}}
EOF
    chmod 0644 "$settings"
    log_ok "settings.json (created with claudebase allowlist)"
    return 0
  fi

  if command -v jq >/dev/null 2>&1; then
    local tmp; tmp="$(mktemp)"
    if jq --arg new "$entry" \
         '(.permissions //= {}) | (.permissions.allow //= []) | .permissions.allow = ((.permissions.allow + [$new]) | unique)' \
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
# Install the claudebase UserPromptSubmit hook into ~/.claude/hooks/ and wire
# it into ~/.claude/settings.json:
#
#   - UserPromptSubmit -> claudebase-selfcheck-reminder.sh — fires before the
#     agent responds, injects a SHORT agent-only reminder covering (1) the three
#     cognitive-self-check protocols and (2) insight-capture: persist any
#     genuine insight from the PREVIOUS turn via `claudebase insight create`.
#
# Migration: a prior version shipped a Stop hook (claudebase-insight-capture)
# that forced reflection via `decision: block` — Claude Code renders that to the
# operator as "Stop hook error: ..." (alarming, looks like a failure) and forces
# an extra turn every response. That approach is RETIRED; insight-capture now
# folds into the UserPromptSubmit reminder (no operator bubble, no "error", no
# extra turn). This installer actively removes the stale Stop wiring + files.
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

  local hook_files=(claudebase-selfcheck-reminder.sh claudebase-selfcheck-reminder.ps1 claudebase-read-insights-reminder.sh claudebase-read-insights-reminder.ps1)
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
    log_warn '  (and remove any hooks.Stop entry pointing at claudebase-insight-capture.sh)'
    return 0
  fi

  local stop_cmd="$HOME/.claude/hooks/claudebase-insight-capture.sh"
  local selfcheck_cmd="$HOME/.claude/hooks/claudebase-selfcheck-reminder.sh"
  local readins_cmd="$HOME/.claude/hooks/claudebase-read-insights-reminder.sh"
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
      '
      .hooks //= {}
      | .hooks.UserPromptSubmit //= []
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
    log_ok "settings.json (UserPromptSubmit[selfcheck] + SessionStart[read-insights] wired; retired Stop[insight-capture] unwired)"
  else
    rm -f "$tmp"
    log_warn "settings.json hook merge failed; please add manually"
  fi
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
      # Darwin/x86_64 dropped as of v0.7.1 — falls through to the catch-all
      # warning below; the upstream pdfium binary release still has it, but
      # since we don't ship the claudebase binary for Intel Mac there's no
      # consumer.
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
# Install Telegram channel bridge wiring (Slice 6 v0.8.0)
# ============================================================================
# The daemon now owns the single Telegram `getUpdates` poller (one bot -> many CLIs).
# This function patches the official Telegram plugin's .mcp.json to run the
# daemon bridge (`claudebase plugin serve`) as the MCP server for the channel.
# The bridge only relays the daemon's notifications (does not poll), so there
# is no dual-poll and NFR-TMC-5 is preserved.
#
# Best-effort: if `claude` CLI is not on PATH or the plugin install fails,
# the function logs and returns 0 (does not abort the installer).
#
# The function is idempotent: re-running rewrites the same .mcp.json.
# To revert, restore .mcp.json from .mcp.json.upstream-backup, or run
# `claude plugin install telegram@claude-plugins-official` to restore
# the upstream version.
install_telegram_channel_bridge() {
  if [ "${CLAUDEBASE_SKIP_TELEGRAM:-0}" = "1" ]; then
    log_info "CLAUDEBASE_SKIP_TELEGRAM=1 — skipping telegram channel bridge wiring"
    return 0
  fi

  if ! command -v claude >/dev/null 2>&1; then
    log_info "claude CLI not on PATH; skipping telegram channel bridge setup"
    log_info "  to install manually after Claude Code is installed:"
    log_info "    claude plugin install telegram@claude-plugins-official"
    log_info "    then re-run this installer to wire the bridge"
    return 0
  fi

  # ----- 1. Install official plugin if not already -----
  log_info "Installing telegram@claude-plugins-official (idempotent)..."
  if ! claude plugin install telegram@claude-plugins-official 2>&1 | tail -2; then
    log_warn "telegram plugin install failed; skipping bridge wiring"
    return 0
  fi

  # ----- 2. Locate the installed plugin dir -----
  # Prefer installed_plugins.json (authoritative — points to the currently
  # ACTIVE version). Fall back to newest-version glob if jq/python3 absent
  # or manifest unreadable.
  local plugin_root="$CLAUDE_DIR/plugins/cache/claude-plugins-official/telegram"
  if [ ! -d "$plugin_root" ]; then
    log_warn "official telegram plugin not found at $plugin_root after install — skipping bridge wiring"
    return 0
  fi

  local plugin_dir=""
  local installed_manifest="$CLAUDE_DIR/plugins/installed_plugins.json"
  if [ -f "$installed_manifest" ]; then
    if command -v jq >/dev/null 2>&1; then
      plugin_dir=$(jq -r '.plugins["telegram@claude-plugins-official"][0].installPath // empty' "$installed_manifest" 2>/dev/null)
    elif command -v python3 >/dev/null 2>&1; then
      plugin_dir=$(python3 -c "import json,sys; d=json.load(open('$installed_manifest')); p=d.get('plugins',{}).get('telegram@claude-plugins-official',[]); print(p[0]['installPath'] if p else '')" 2>/dev/null)
    fi
  fi

  if [ -z "$plugin_dir" ] || [ ! -d "$plugin_dir" ]; then
    # Fallback: newest version subdir (semver-sortable).
    local version_dir
    version_dir=$(ls -1 "$plugin_root" 2>/dev/null | sort -V | tail -1)
    if [ -z "$version_dir" ] || [ ! -d "$plugin_root/$version_dir" ]; then
      log_warn "no version subdir found under $plugin_root — skipping bridge wiring"
      return 0
    fi
    plugin_dir="$plugin_root/$version_dir"
    log_info "manifest lookup unavailable; falling back to newest-version glob: $plugin_dir"
  fi
  log_info "wiring daemon bridge at $plugin_dir"

  # ----- 3. Back up upstream .mcp.json and patch with daemon bridge -----
  local mcp_json="$plugin_dir/.mcp.json"
  local mcp_backup="$plugin_dir/.mcp.json.upstream-backup"

  if [ -f "$mcp_json" ] && [ ! -f "$mcp_backup" ]; then
    cp "$mcp_json" "$mcp_backup"
    log_ok ".mcp.json.upstream-backup preserved"
  fi

  # Write .mcp.json to run the daemon bridge as the MCP server for the
  # telegram channel. The bridge relays the daemon (does not poll), so
  # there is no dual-poll and NFR-TMC-5 is preserved.
  cat > "$mcp_json" <<'EOF'
{
  "mcpServers": {
    "telegram": {
      "command": "${HOME}/.claude/tools/claudebase/claudebase",
      "args": ["plugin", "serve"]
    }
  }
}
EOF
  chmod 0644 "$mcp_json"
  log_ok ".mcp.json patched (daemon bridge relays the daemon poller, no dual-poll)"

  log_info "to enable: launch Claude Code with"
  log_info "  claude --channels plugin:telegram@claude-plugins-official"
  log_info "or use the shorthand:"
  log_info "  claudebase run"
  log_info "to revert: restore .mcp.json from .mcp.json.upstream-backup, or"
  log_info "  claude plugin install telegram@claude-plugins-official"
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
echo "    rules/              (4 files — cognitive-self-check, knowledge-base, knowledge-base-tool, tool-limitations)"
echo "    commands/           (4 files — knowledge-ingest, reflect, consolidate, update-claudebase)"
echo "    agents/             (2 files — reflection, consolidator)"
echo "    hooks/              (1 hook — UserPromptSubmit[self-check + insight-capture])"
echo ""

if ! confirm "Proceed with installation?"; then
  log_info "Aborted."
  exit 0
fi

get_source_dir
install_prompts
install_binary
register_alias
register_bash_allowlist
install_claudebase_hooks
install_pdfium
install_whisper_stack
preload_encoder
install_telegram_channel_bridge

# ============================================================================
# Optional post-install daemon hook (Slice 2 — STRUCTURAL-2-3)
# ============================================================================
# Opt-in via `CLAUDEBASE_INSTALL_DAEMON=1`. Fails soft: the post-install
# step never aborts the installer when it errors. `--no-start` keeps the
# install pre-reboot — the service comes up on the next login (systemd
# user unit + WantedBy=default.target).
if [ "${CLAUDEBASE_INSTALL_DAEMON:-0}" = "1" ]; then
  log_info "CLAUDEBASE_INSTALL_DAEMON=1 detected; installing daemon service unit..."
  if claudebase daemon install --no-start --yes; then
    log_ok "Daemon service unit installed (start at next login or via 'claudebase daemon start')"
  else
    log_warn "Daemon install failed; continuing without daemon (re-run later with 'claudebase daemon install')"
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
echo "  Skills installed:"
echo "    /knowledge-ingest    Ingest a folder/file into the per-project knowledge base"
echo "    /reflect             DMN unfocused observation pass — user-invoked"
echo "    /consolidate         Cross-artifact drift detection (auto-chained between waves)"
echo ""
echo "  Agents installed:"
echo "    reflection (Drift)       Default Mode Network observation pass"
echo "    consolidator (Mnem)      Hippocampal-replay drift detection"
echo ""
echo "  Tip: re-ingest existing PDFs (\`claudebase ingest <path>\`) to upgrade"
echo "  pre-v2 indexes to schema v3 — that's what unlocks per-page citations."
echo ""
