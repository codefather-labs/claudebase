# Command: Update claudebase

Update the locally-installed `claudebase` to the latest version by reading the **authoritative, current install instructions from the repository README** and following them — rather than running a hardcoded command that may have drifted from how the installer actually works today.

The reason this command reads the README instead of baking in commands: the install / update procedure evolves (new opt-out flags, new install steps, changed one-liner URL). The README at the repo root is the single source of truth and is updated with every release. This command's job is to fetch that truth, then execute it correctly for the current machine.

## When to invoke

- The operator says "update claudebase" / "get the latest claudebase" / "обнови клодбейс".
- After a new claudebase release lands and the operator wants the new binary + refreshed agent toolkit (rules / commands / agents / hooks).
- When a bug the operator hit was fixed upstream and they want the fix locally.

NOT auto-chained from anything — exclusively operator-invoked.

## Protocol

### Step 1 — Capture the current state

```
claudebase --version    # note the installed version, e.g. "claudebase 0.6.0"
```

If the binary is absent (`command -v claudebase` empty AND `~/.claude/tools/claudebase/claudebase` not executable), this is a fresh INSTALL, not an update — still proceed; the same installer handles both.

### Step 2 — Read the authoritative current README

Fetch the live README from the repository — do NOT rely on memory of how the installer worked:

- **WebFetch** `https://github.com/codefather-labs/claudebase` (or the raw README at `https://raw.githubusercontent.com/codefather-labs/claudebase/main/README.md`) and read the **"Quick install"** section. That section is the canonical install/update procedure.
- If a local checkout of the repo exists (a directory containing `install.sh` whose `git remote -v` points at `codefather-labs/claudebase`), `git pull` it first and read its `README.md` directly — the checkout's README on `main` is equally authoritative and avoids a network round-trip.

Extract from the README: the exact remote install one-liner, the local-checkout invocation, the opt-out environment variables, and any new steps the installer now performs.

> As of this command's authoring the README documented these (CONFIRM against the live README — they may have changed):
> - Remote, Linux/macOS: `curl -fsSL https://raw.githubusercontent.com/codefather-labs/claudebase/main/install.sh | bash -s -- --yes`
> - Remote, Windows (PowerShell): `iwr -useb https://raw.githubusercontent.com/codefather-labs/claudebase/main/install.ps1 | iex`
> - Local checkout: `bash install.sh --local --yes` (Unix) / `.\install.ps1 -Yes -Local` (Windows)
> - Opt-outs: `CLAUDEBASE_SKIP_WHISPER=1`, `CLAUDEBASE_SKIP_TELEGRAM=1`
> The installer is idempotent + version-aware: it downloads the latest release binary only if newer, redeploys the agent toolkit (rules / commands / agents / hooks) into `~/.claude/`, and re-merges `settings.json` hook wiring without duplicating entries. No Rust toolchain required.

### Step 3 — Choose the path that matches this machine

- **Local checkout present** → `git pull` (NEVER `git rebase` — per the git workflow rule), then run the README's local-checkout command (`bash install.sh --local --yes` / `.\install.ps1 -Yes -Local`). This is the path for contributors working on claudebase itself.
- **No checkout** → run the README's remote one-liner for the current OS.
- **Honor the operator's opt-out env vars** if they previously set `CLAUDEBASE_SKIP_WHISPER` / `CLAUDEBASE_SKIP_TELEGRAM` — preserve them on the update invocation so the update doesn't re-enable a subsystem they disabled.

Run the installer. Surface its output; if it warns (e.g. binary download failed), relay the warning and the README's documented fallback rather than silently continuing.

### Step 4 — Verify the update landed

```
claudebase --version                                  # new version > old version (or unchanged if already latest)
claudebase status --json --project-root <a project>   # binary runs, schema intact
jq '.hooks | keys' ~/.claude/settings.json            # Stop + UserPromptSubmit still wired (idempotent re-merge)
ls ~/.claude/rules/ ~/.claude/commands/ ~/.claude/agents/   # toolkit refreshed
```

If `--version` is unchanged AND it was already the latest release, that is a correct no-op outcome — report it as "already up to date", not a failure.

### Step 5 — Report what changed

- State the version delta (`0.6.0 → 0.7.0`, or "already at latest 0.x.y").
- Skim the repo `CHANGELOG.md` `[Unreleased]` + the newest released section (or the GitHub release notes) and summarize the user-facing changes the operator just received in 3-5 bullets.
- If new commands / rules / hooks were added by this update, name them so the operator knows what is now available.

## Safety

- **Never `git rebase`** on a local checkout — `git pull` (merge/fast-forward) only, per the git workflow rule. If the pull conflicts, stop and surface it; do not force.
- **Never** pass `--force` to any install/git step, and never `npm publish` / `cargo publish` / release-cutting from an update command — updating is consumption, not publishing.
- The installer is the authority on WHAT gets installed; this command only decides the correct invocation. Do not hand-copy binaries or hand-edit `settings.json` — let the installer do it idempotently.

## Output (when /update-claudebase completes)

```markdown
## claudebase update

- **Version:** <old> -> <new>   (or: already at latest <ver>)
- **Path used:** local checkout (git pull + install.sh --local) | remote one-liner
- **Toolkit refreshed:** rules / commands / agents / hooks redeployed; settings.json hooks re-merged (no dupes)
- **What changed:** <3-5 bullets from CHANGELOG / release notes>
- **New surfaces (if any):** <new commands / rules / hooks now available>
```
