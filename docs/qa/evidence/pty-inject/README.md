# Slice 0 evidence — PTY injection into a live `claude` TUI

**Date:** 2026-08-16 · **Host:** Linux 7.0.0-29-generic · **Claude Code:** 2.1.233
**Spike:** `spikes/pty_inject/` (`portable-pty 0.9`, standalone crate)

**Verdict: GATE PASSED.** Injecting into a `claude` TUI by owning its PTY works, including
submission. The plan `docs/plans/claudebase-v0.10-pty-transport.md` may proceed to Slice 1.

## What was run

| Run | Command | Result |
|---|---|---|
| selftest | `--selftest` (child = `cat`) | marker round-tripped off the master in 101 ms → PTY plumbing sound |
| run1 | `--mode paste --submit cr` (CR written in the same buffer), delay 15 s | **injection eaten by a startup modal** ("Claude in Chrome detected") — the CR confirmed option 1 |
| run2 | same, delay 20 s, marker `PINGMARKER42` | text **landed in the input line**, bracketed-paste markers not echoed → TUI honours DECSET 2004; **no submit** |
| run3 | `--mode paste --submit cr --submit-delay-ms 400`, delay 18 s, marker `PINGMARKER43` | text landed AND **submitted**: `❯ PINGMARKER43 …` → `● пинг`, "Churned for 2s" |

## Findings (these drive Slices 2 and 3)

- **F-1 — bracketed paste is the right frame.** `ESC[200~ … ESC[201~` is consumed, not echoed;
  multi-line content stays one atomic paste instead of N submissions.
- **F-2 — the submit key must be a SEPARATE, LATER write.** A `\r` in the same buffer as the paste
  does nothing (run2); the same `\r` 400 ms later submits (run3). The supervisor must therefore
  treat "paste" and "submit" as two timed writes, not one payload. 400 ms is a working value, not a
  measured minimum — the production code should re-check the boundary and pick a margin.
- **F-3 — modal dialogs swallow injections, and a stray CR ANSWERS them.** Run1's CR confirmed
  "Yes, use my browser" and launched a browser on the operator's desktop. Two consequences for the
  supervisor: (a) never inject before the TUI is at its input prompt; (b) a bare submit key is a
  destructive act when a modal is up. CC raises modals mid-session too, not only at boot (run3
  ended on "Set up auto mode for your environment?"), so readiness detection cannot be a one-shot
  startup check — it has to gate every injection.
- **F-4 — no controlling TTY on the supervisor is not a blocker for inbound.** All three runs had
  a non-TTY stdin (agent shell); the child still got a real PTY and accepted injection. Degradation
  to "no injection" is only needed for the operator-facing proxy half, not for delivery.
- **F-5 — nested sessions inherit `CLAUDE_CODE_CHILD_SESSION`** and disable transcript saving in the
  child. Irrelevant in production (the supervisor's child is a normal top-level session) but it is
  why these transcripts show the warning.

## Slice 3 scenarios (run 2026-08-16, same session)

The spike scripts the operator's side too: bytes it writes and bytes a human types enter the child's
input queue through the same path, so `Step::Type` is a faithful keyboard stand-in.

| Scenario | Question | Result |
|---|---|---|
| `busy` | inject WHILE the model is generating | ✅ **queued, not lost** — `MK-B` was injected 2.5 s into a 40-number generation, waited for `MK-A` to finish, then submitted and answered `● браво` (`scenario-busy-transcript.txt`) |
| `multiline` | does a 3-line block submit per-line? | ✅ **atomic** — all three lines landed as ONE message and produced one answer `● чарли` (`scenario-multiline-transcript.txt`) |
| `typing` | inject on top of a half-typed operator line | ❌ **silent concatenation** — submitted message was `MK-OP недописанная строка оператораMK-B ответь ровно одним словом: браво`; the operator's unsent draft was swallowed into the inbound message and sent (`scenario-typing-transcript.txt`) |
| `busy` (1st attempt) | — | inconclusive: a mid-session modal ate it, see F-3 below |

### F-6 — injecting over a non-empty input line concatenates, silently

Not a crash, which is worse: the operator's private draft is merged into the inbound message and
submitted as one prompt. Nothing warns anyone. **The "quiet window" from the plan is therefore not
optional, and it should not be a timer heuristic at all** — the supervisor proxies every operator
keystroke, so it can track draft state exactly:

- printable byte / paste from the operator → draft dirty
- `\r` / `\n` (submit), `Ctrl-C` (0x03), `Ctrl-U` (0x15) → draft clean
- backspace (0x7f) → decrement, clean at zero
- arrow keys / history recall → treat as dirty (text can appear without a printable keystroke)

While dirty: queue inbound messages, coalesce them, and flush when the line goes clean. This is
deterministic, not a guess — the supervisor is the only path those keystrokes travel.

### F-3 (reconfirmed, upgraded severity) — modals eat messages AND the submit key drives them

Reproduced twice more. In the first `busy` attempt the "Set up auto mode for your environment?" modal
appeared mid-session: `MK-B` occurs **0 times** in that transcript (message lost entirely) and our CR
advanced the wizard to its second page. Running the child the way production will
(`-- --dangerously-skip-permissions`, which `claudebase run` passes by default) removed that modal and
the same scenario passed cleanly — but permission dialogs are not the only modals CC raises, so the
supervisor still needs a modal guard before every injection, plus an "unsent" queue rather than a
best-effort write.

## Not yet answered

- Windows / ConPTY — untested (plan risk R-3).
- Long-running modal states (e.g. a `/`-menu open) — only auto-mode and chrome onboarding were
  observed; the detector's signature list needs to grow from real usage.

## Files

- `selftest.log`, `run1-…log`, `run2-…log`, `run3-…log` — spike's own timestamped log
- `run3-transcript-stripped.txt` — child's PTY output with ANSI/OSC stripped; contains the
  `❯ PINGMARKER43 …` input line and the `● пинг` answer

## Reproduce

```bash
cd spikes/pty_inject
cargo build --release
./target/release/pty-inject-spike --selftest            # no TTY needed, CI-safe
TERM=xterm-256color ./target/release/pty-inject-spike \
    --cmd claude --delay-ms 18000 --mode paste --submit cr --submit-delay-ms 400 \
    --text "PINGMARKER ответь одним словом: пинг"

# Slice 3 scenarios — pass the production flag so mid-session modals don't skew the run
for s in plain busy typing multiline; do
  TERM=xterm-256color timeout 90 ./target/release/pty-inject-spike \
      --cmd claude --scenario "$s" --delay-ms 16000 --submit-delay-ms 400 \
      --log "/tmp/sc-$s.log" -- --dangerously-skip-permissions > "/tmp/sc-$s.out" 2>&1
done
```

Transcripts are raw PTY output; strip ANSI before reading (`sed`/`python re`) — the `*-transcript.txt`
files here are already stripped.
