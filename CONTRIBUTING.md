# Contributing to claudebase

Thanks for considering a contribution! claudebase is a small focused tool — the codebase is approachable and most changes don't need to touch many files.

## Quick start

```sh
# Clone + install locally (uses the local checkout instead of downloading the binary)
git clone https://github.com/codefather-labs/claudebase
cd claudebase
bash install.sh --local --yes        # Linux / macOS
# or
powershell -NoProfile -ExecutionPolicy Bypass -File install.ps1 -Yes -Local    # Windows
```

This installs claudebase in dev mode into `~/.claude/tools/claudebase/`, registers the `claudebase` alias on PATH, and seeds the agent toolkit (rules / commands / agents) into `~/.claude/`.

## Development loop

```sh
# Fast iteration — typecheck only
cargo check --workspace

# Full test pass
cargo test --workspace

# Build the release binary (used by install.sh in --local mode)
cargo build --release --workspace

# Run the CLI directly without re-installing
./target/release/claudebase --help
./target/release/claudebase search "your query"
```

## Branching + commits

- Work on a feature branch: `feat/<slug>` or `fix/<slug>`. **Never commit directly to `main`** — open a PR.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):
  - `feat(scope): message` — new feature
  - `fix(scope): message` — bug fix
  - `docs(scope): message` — docs only
  - `chore(scope): message` — tooling / housekeeping
  - `refactor(scope): message` — no behavior change
  - `test(scope): message` — adding / fixing tests
- Scopes in this repo: `core` | `infra` | `daemon` | `plugin` | `cli`
- One slice / one logical change = one commit. Squash when needed via interactive rebase **before** the PR review starts.
- No "Co-Authored-By" or AI attribution in commit messages.

## Code style

- `cargo fmt` MUST be clean before pushing.
- `cargo clippy --workspace --all-targets` MUST be clean (or new warnings explicitly justified in the PR body).
- Prefer composition over inheritance, return `Result<_, anyhow::Error>` for fallible application code, surface concrete error types in library code.
- Keep new dependencies behind feature flags when possible. The release binary should stay under ~35 MB stripped (enforced by `.github/workflows/release.yml`).
- Tracing instead of `println!` / `eprintln!` for all logging.

## Tests

- Unit tests live next to source code under `#[cfg(test)] mod tests`.
- Integration tests live under `tests/` at the workspace root.
- Anything touching SQLite / fastembed / pdfium needs an integration test under `tests/`; unit tests can mock smaller pieces.
- `cargo test --workspace` is the gate — all tests MUST pass locally before opening the PR.

## Docs

- README is the operator-facing entry point — keep it scannable.
- Design docs and plans live in `docs/` and `docs/plans/`. Multi-file features should get a `docs/plans/<feature-slug>.md` plan written FIRST, then implemented.
- If you change user-visible behavior, update README accordingly in the same PR.
- If you change a wire format / config schema, bump `schema_version` in the affected file and document the migration.

## Pull request expectations

1. PR template is auto-populated when you open the PR — fill it out, don't delete sections.
2. CI must pass (build matrix across darwin-arm64, darwin-x64, linux-x64, linux-arm64, windows-x64 + smoke test).
3. Reviewer will look for: tests, docs alignment, no new clippy warnings, conventional commit hygiene.
4. Squash-merge is the default (preserves linear history on `main`).

## Releasing

See [RELEASING.md](RELEASING.md). Tags follow `claudebase-v<MAJOR>.<MINOR>.<PATCH>`; pushing a tag triggers the release workflow which builds all 5 platform binaries + uploads to GH Releases.

## Reporting bugs / asking questions

- 🐛 Bug → use the bug-report issue template
- ✨ Feature idea → use the feature-request issue template
- 🔌 Plugin / Claude Code integration question → use the plugin-question template
- 💬 Open-ended question → start a [Discussion](https://github.com/codefather-labs/claudebase/discussions) instead of an issue
- 🔐 Security vulnerability → see [SECURITY.md](SECURITY.md), do NOT open a public issue

## Project values

- **Local-first.** No external API calls in the hot path. The binary works without internet (once installed).
- **Single binary.** No Python deps, no Node deps, no JVM. Single Rust static binary that drops onto any machine.
- **Honest about state.** Tracing logs are structured, errors are surfaced loud, no silent fallbacks that hide real problems.
- **Minimal surface.** Add a feature when it earns its keep; remove it when it doesn't.
