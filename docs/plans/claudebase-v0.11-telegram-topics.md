# Telegram topics as session addresses

**Status:** Slice 0 done and its findings fixed; Slice 1 (the group access gate) remains the blocker before anyone else joins a topics group
**Supersedes:** the bot-pool proposal (one bot token per session), rejected below.

## The problem

One bot, one DM, and `/switch` as a mode. The operator has to remember which
session the chat currently points at, and every message is sent on the strength
of that memory. When the memory is wrong the message goes to the wrong session —
which happened twice in one evening on 2026-08-18, once in each direction.

A mode that has to be held in the operator's head is the wrong shape for
something that is switched dozens of times a day.

## The rejected alternative: a pool of bot tokens

Give each session its own bot from a pre-registered pool, so each session gets
its own private chat. It works, and it was the first proposal. It was rejected
because the cost is paid per session, forever:

- one BotFather registration and one token to store, guard and rotate per bot;
- one `/start` per bot, because a bot cannot open a conversation — N one-time
  manual steps that land on the operator, not the daemon;
- one long-poll loop per bot;
- a lease/expiry/reclaim mechanism, plus a sticky lease per nick, or a session
  changes chat on restart and its history scatters;
- a hard ceiling on concurrent sessions, at the number of bots registered.

All of it to obtain something one bot in a forum already provides.

## The design

One bot, one supergroup with topics enabled. **A topic is a session's address.**

- The operator creates a forum supergroup and adds the bot.
- In a topic, the operator taps a button and picks a session from the list of
  live ones. That topic is now bound to that session.
- Everything the operator types or dictates in that topic goes to that session.
  Everything the session sends comes back into that topic.
- `/switch` stops being needed. There is no current session, because there is no
  mode: the topic the operator is typing in *is* the address.

The cost is one supergroup and one button-tap per session. Nothing scales with
the number of sessions except the topics themselves, which is the point.

## What already exists

Verified in the tree, and re-verified on 2026-08-19 after a day of unrelated edits
(every line reference below moved; every claim held):

| Piece | Where |
|---|---|
| Bindings keyed by `(chat_id, thread_id)` — per topic, not per chat | `src/daemon/chat.rs:662` |
| `handle_switch(tx, chat_id, thread_id, …)` already takes the topic | `src/daemon/telegram.rs:506` |
| `/start` → `[agents, switch]` keyboard → one button per live session → `startswitch:<name>` → binds | `src/daemon/telegram.rs:906` |
| `message_thread_id` parsed off the inbound message and carried into notification meta | `src/daemon/telegram.rs:322` |
| Outbound and bot replies carry `Option<i64> thread_id` | `BatchOutcome::pair_replies`, `BatchOutcome::pair_replies` |
| `agent_registry.routing_thread_id` — the reverse lookup, session → topic | `src/daemon/agent_registry.rs:1357` |
| A supergroup forum-topic update shape already under test | `src/daemon/telegram.rs:2865` |
| `Chat` carries only `id` — the daemon never asks whether a chat is a DM | `src/daemon/telegram.rs:334` |

The button flow the design calls for is the flow `/start` already implements.
The addressing the design calls for is the addressing `chat_bindings` already
stores. **This is mostly a verification and gap-closing job, not a build.**

## Slice 0 — DONE, driven live on 2026-08-19

A forum supergroup with two topics, bot added and promoted to administrator.
Answers, in the order the questions were asked:

1. **Does the bot see ordinary messages in a topic?** Yes, once promoted to
   administrator. Before promotion only `/start@botname` arrived — exactly the
   privacy-mode signature R-1 predicted, and it presents as silence.
2. **Does `/start` reply into the topic?** Yes: `keyboard sent chat_id=-100…
   thread_id=Some(3)`. It would have been `None` before the fix landed earlier
   the same day.
3. **Does tapping bind `(chat_id, topic)`?** Yes: `switch applied chat_id=-100…
   thread_id=Some(3) agent=transport`, and both tables agree —
   `chat_bindings (-100…, 3) -> transport` and
   `agent_registry.routing_thread_id = 3`.
4. **Does a message typed in the topic reach the bound session?** Yes.
5. **Does that session's reply come back into the same topic?** NOT AT FIRST —
   it went to General. `resolve_thread` read the binding as
   `(chat_id, _thread_id)` and discarded the topic one line before it would
   have been used. Fixed; the send now logs `thread_id: Some(3)`.
6. **Voice note in a topic?** Not yet driven.
7. **Two topics, two sessions?** Driven, and it FAILED where reading said it
   would work. The switch is indeed per-topic in both tables — that part of the
   reading was right. What was wrong is one layer down: ownership was asked
   about the CHAT, so `deliver_backlog` elected a single owner for the whole
   forum and discarded the other session's messages. The peer session confirmed
   it had received nothing at all. Fixed by storing the topic on the message row
   and deciding delivery per message; `routing_belongs_to(agent, chat, topic)`
   is the question that has an answer.

   The lesson is worth keeping: "the switch is per-topic" was true and still
   did not mean "two topics work". Reading verified the layer it looked at.

**Limitation found while answering 7:** the live routing key is a single pair of
columns on the agent's row, so one session can hold exactly ONE topic. Many
topics to many sessions works; one session across several topics does not — the
second bind moves it and leaves the first topic with a durable `chat_bindings`
row and no live addressee.

## Slice 0 — the original questions

Before writing code: create the forum, add the bot, and drive it by hand.
Everything below is a question with a yes/no answer, and each answer either
deletes a slice or defines it.

1. Does the bot see ordinary messages in a topic at all? (R-1 says yes IF it was
   promoted to administrator — this question is now "was the setup done right",
   not "is the design possible".)
2. Does `/start` in a topic reply *into* that topic, or into the group's General?
3. Does tapping a session in that keyboard bind `(chat_id, topic_id)`? —
   **ANSWERED BY READING, AND IT DID NOT.** The callback handler passed `None`
   for the thread into `handle_switch`, so a tap bound the whole chat and every
   topic in the forum resolved to whichever session was assigned last: the
   `/switch` mode this design exists to remove, wearing a button. `MessageRef`
   did not even carry `message_thread_id`, so the information was not available
   to pass. Fixed 2026-08-19; the remaining question is whether it behaves in a
   real forum.
4. Does a message typed in the topic reach the bound session?
5. Does that session's reply come back into the same topic?
6. Does a voice note in a topic transcribe and arrive?
7. What happens in a second topic bound to a second session — do the two stay
   apart?

Record the answers in this file before starting Slice 1.

## Slices

**Slice 1 — the gate for groups. This is a blocker, not a preference.**

`gate_dm` (`src/daemon/channel_state.rs:291`) decides on the SENDER alone and
never asks what kind of chat the message came from. Read against a forum, that
gives:

- the operator, already allowlisted, passes — messages in a topic reach their
  session, which is what the design needs;
- **any other group member is an unknown sender, so pairing fires, and the
  pairing code is replied into the chat the message came from — the topic.**

In a DM that reply is private by construction; the sender is the only other
party. In a group it publishes the credential into a space every member reads,
so anyone the operator admitted can pair themselves by reading what the bot just
posted. The gate does not fail closed here; it fails loudly and publicly.

For a supergroup with one member this is moot. It stops being moot the first
time anyone else is added, and nothing in the current code marks that moment —
which is exactly why this cannot be left to be discovered.

Minimum for the slice: the group must be gated by CHAT, not by sender-pairing.
An allowlisted `chat_id` (the forum) plus the existing per-user allowlist is the
likely shape, with pairing suppressed entirely outside DMs — a group is joined
by invitation, and the invitation IS the pairing decision, so re-deriving it
from a code in the channel adds nothing but a leak.

Found by reading rather than by driving the forum, which is why it belongs here
before Slice 0 rather than in its answers.

**Slice 2 — close whatever Slice 0 found.** Partly done before Slice 0 ran,
because reading found it: the assignment button, its keyboard, the `/agents`
reply and the switch confirmation all passed `None` for the topic and therefore
addressed the chat. All four now carry the topic the tap happened in. What
remains for this slice is whatever driving a real forum turns up.

**Slice 3 — assignment as a first-class action.** `/start` works but reads as
onboarding. A `/assign` (same keyboard, clearer name), plus `/whoami` in a topic
naming the bound session, plus a visible message when a bound session dies —
otherwise a topic silently becomes a dead letter box.

**Slice 4 — unbinding and lifecycle.** A session that exits should release its
topic and say so in it. A topic bound to a session that never comes back should
be re-assignable without a restart.

**Slice 5 — docs and the skill.** The forum setup is a one-time manual procedure
with at least one non-obvious step (R-1). It has to be written down, in the
installer's onboarding and in a skill, or every new machine rediscovers it.

## Risks

**R-1 (RESOLVED 2026-08-19): bot privacy mode.** It was real, and it is one
setting away.

Privacy mode is **enabled by default**, and a bot in that state receives only
commands addressed to it, replies to its own messages, and service messages —
not ordinary typed text and not voice notes. Without changing it, every topic
would look connected and deliver nothing, and the failure would present as
silence rather than as an error.

Two ways out, and they are not equivalent:

- `/setprivacy` in BotFather turns it off **for the bot**, in every group it will
  ever join.
- **Adding the bot as an administrator of the forum** exempts it: "bots that
  were added to a group as admins" always receive all messages.

Take the second. It is scoped to the one supergroup instead of to the bot's
whole future, and admin rights are needed anyway the moment the daemon wants to
create or manage topics itself. Source:
https://core.telegram.org/bots/features (read 2026-08-19).

The setup step is therefore: create the forum supergroup, add the bot, and
promote it to administrator. Slice 5 has to say exactly that, because a bot
added without admin rights produces a working-looking setup that silently
delivers nothing.

**R-2 (medium): one poll loop, many topics.** All topics share the single
long-poll. That is the design's main advantage over the pool, and it means
anything that blocks the loop blocks every topic at once. Voice transcription
used to do exactly that; it was moved off the loop on 2026-08-18. Nothing else
may move onto it.

**R-3 (medium): the group is a shared surface.** In a DM the operator is alone.
In a group, anyone admitted can read every topic and address every session.
Membership becomes an access-control decision — see Slice 1.

**R-4 (low): topics are a supergroup feature.** The design does not work in a
plain group or a DM. The fallback for a machine without a forum is today's
`/switch`, which is why `/switch` should be kept rather than removed.

## Facts

### Verified facts
- Bindings are already per-topic: `chat_bindings` PK is `(chat_id, thread_id)` — source: `src/daemon/chat.rs:633-639` — salience: high
- `/start` already renders a per-session button keyboard whose taps call `handle_switch` — source: `src/daemon/telegram.rs:426-429`, `:906` — salience: high
- `handle_switch` already accepts `thread_id: Option<i64>` — source: `src/daemon/telegram.rs:506-512` — salience: high
- The daemon never inspects chat type; `Chat` deserialises `id` only — source: `src/daemon/telegram.rs:333-335` — salience: medium
- A supergroup forum-topic update shape is already exercised by a test — source: `src/daemon/telegram.rs:2713-2719` — salience: medium
- The operator sent a message to the wrong session twice on 2026-08-18 under the `/switch` model — source: this session's transcript — salience: medium

### External contracts
- Telegram Bot API — privacy mode is ON by default; a privacy-enabled bot sees only commands addressed to it, replies and service messages; bots added as group ADMINS always receive all messages — source: https://core.telegram.org/bots/features (read 2026-08-19) — verified: yes — salience: high
- Telegram Bot API — `message_thread_id` identifies a forum topic on both inbound and outbound — source: `src/daemon/telegram.rs:315` (in-tree usage, live-tested for inbound) — verified: partial — salience: high

### Assumptions
- The existing `/start` keyboard path passes the topic id into its reply — risk: the keyboard lands in General and the operator cannot assign from inside a topic — how to verify: Slice 0 question 2 — salience: high
- A session's outbound reply resolves its topic from `routing_thread_id` — risk: replies land in General and every topic sees every session — how to verify: Slice 0 question 5 — salience: high

### Open questions
- Confirmed as a defect rather than a question: DM pairing applied to a group publishes the pairing code into the group (see Slice 1). What remains for the user is the policy shape, not whether to change it — salience: high
- Should `/switch` remain for DM use, or be removed once topics work? — needs: user decision — salience: low

## Decisions

### Inbound validation
- Task arrived as "plan proxying Claude conversations onto Telegram topics, assigned by a button" — challenged: yes, against the bot-pool proposal the same operator made an hour earlier — outcome: proceeded, and recorded why the pool was dropped, because the two proposals conflict and a plan that silently picks one is unreviewable — salience: high
- The task was framed as building a proxy layer; the tree shows the addressing and the button flow already exist — outcome: pushed back on the framing and restructured the plan around verifying first — salience: high

### Decisions made
- Topics over a bot pool — alternatives considered and rejected: pool of bot tokens (per-session cost in tokens, onboarding, poll loops, lease machinery, and a hard ceiling on sessions); keeping `/switch` (the mode is the defect) — Q1-Q5: hack? no | sane? yes, it removes machinery rather than adding it | alternatives? both listed | symptom-or-cause? cause — the defect is that addressing is a mode, and a topic makes it an address | root-cause-tracked? n/a — salience: high
- Slice 0 is verification, not implementation — Q1-Q5: hack? no | sane? yes | alternatives? writing the slices blind, rejected because most of them may already be done | symptom-or-cause? cause | root-cause-tracked? n/a — salience: medium
- `/switch` is kept, not removed — a machine without a forum supergroup has no other addressing — salience: medium

### Hacks / workarounds acknowledged
- (none)

### Symptom-only patches (with root-cause links)
- (none)
