# Public API of the journal and mind maps

Status: implemented on branch `feat/orchestrator-api`.

For programs that are not Claude sessions, such as an orchestrator. It shares the callback listener.

```bash
claudebase daemon callback enable --bind 127.0.0.1:8585
claudebase daemon restart
claudebase daemon api issue orchestrator     # prints the token once
```

Every request carries `X-Api-Token`. The token reaches every project on this daemon.

| Route | Purpose |
| --- | --- |
| `GET /v1/version` | `{"api": 1, "claudebase": "<version>"}` |
| `GET /v1/projects` | every journal on this daemon |
| `POST /v1/journal/<op>` | `add`, `search`, `list`, `get`, `tags`, `categories`, `mark`, `unmark`, `close`, `reopen`, `agenda`, `link`, `unlink`, `branch`, `policy` |
| `POST /v1/mindmap/<op>` | every mind map operation except `send`, `url`, `export`, `import` |

The body is a JSON object with the arguments of the matching `journal_*` tool, plus:

- `project_id`: required, the repository as `host/owner/repo`. Any spelling of a remote URL is normalised as a session's is, so `GitHub.com/Acme/App.git` and `git@github.com:acme/app` reach the same journal that sessions of that repository use. What has no host, owner and repo is refused.
- `author`: required, a name. It is the entry's agent and the owner of a private map.

There is no session. `agent_id`, `session_token`, `cwd` and `every_secs` in the body are ignored. An entry's agent is always its `author`; on `list` and `search`, `agent` is the filter it is for a session, and reads are not narrowed to the author's own entries.

A project's journal starts with its first `journal/add` or `mindmap/new`. Any other call on a project with no journal answers `404 {"error": "unknown_project"}` and creates nothing, so a misspelt project is told and not silently forked.

`journal/policy` reads the policy; setting it is refused, since it is the operator's (`claudebase journal policy` in a plain terminal).

Answers: `200` with the tool's JSON; `400 {"error": "refused" | "bad_request", "message"}`; `401`, before the body is read; `404 {"error": "not_found" | "unknown_project"}`; `413` over 4 MiB;
`500 {"error": "storage", "message"}` to retry.

The version changes only with a change an existing client would notice.

The listener is plain HTTP: keep it on loopback or a private network, and put TLS in front of it otherwise.
