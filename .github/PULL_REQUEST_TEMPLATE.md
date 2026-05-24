<!--
Thanks for the PR! A few things to confirm before reviewers can pick this up:
-->

## Summary

<!-- One paragraph: what changed and why. Link the issue it closes if any. -->

Closes #

## Type of change

- [ ] 🐛 Bug fix (no API change)
- [ ] ✨ New feature (no breaking API change)
- [ ] 💥 Breaking change (API / wire format / config change)
- [ ] ♻️ Refactor (no behavior change)
- [ ] 📝 Docs only
- [ ] 🔧 Infra / CI / tooling

## Checklist

- [ ] `cargo fmt` clean
- [ ] `cargo clippy --workspace --all-targets` clean (or new warnings justified in PR body)
- [ ] `cargo test --workspace` passes locally
- [ ] `bash install.sh --local --yes` still smokes successfully on macOS / Linux
- [ ] If the change touches plugin / wire format: smoke-tested end-to-end in Claude Code
- [ ] If the change touches user-facing behavior: README and / or `docs/` updated
- [ ] If the change is a breaking API change: `CHANGELOG.md` `[Unreleased]` updated under `Changed` or `Removed`
- [ ] Commit messages follow Conventional Commits (`feat(scope):`, `fix(scope):`, `docs(scope):`, `chore(scope):`)

## Manual verification

<!-- Reproducible steps the reviewer can run to verify. Paste output if it helps. -->

## Risks + rollback

<!-- What could go wrong? How would we roll back if it does? -->
