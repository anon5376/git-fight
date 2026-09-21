# Architecture

git fight online is a GitHub App plus a browser game. Someone comments `/fight` on a conflicted pull request. The app names the two people whose code collides, opens a live match, and lets teammates watch. Each round is one conflict. If every round has a winner, the app pushes a resolution to a **new** branch for humans to review. It never merges the PR.

This document is the Milestone 0 plan. Later milestones implement it. Hard rules here apply to every milestone.

## Hard rules

- The bot's only GitHub writes are issue/PR comments and **new** branches named `git-fight/pr-<number>-<match-id>`. It never pushes to an existing branch, never force-pushes, and never merges a PR.
- A draw or forfeit leaves that conflict unresolved. If any conflict in a match is unresolved, the bot pushes nothing and says so.
- Never run code from a user's repo. Server-side git is plumbing only: clone, fetch, merge-tree, cat-file, hash-object, commit-tree, push. Every git invocation sets `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, and `-c core.hooksPath=/dev/null`.
- Verify every webhook's `X-Hub-Signature-256` with a constant-time comparison **before** parsing the body.
- Least privilege: the app asks only for the permissions in [GitHub App](#github-app).
- Secrets (app private key, webhook secret, OAuth client secret, session key) come from environment variables. Never commit them, log them, or store them in the database.
- Create installation tokens when needed and cache them in memory until they expire. Never write them to disk.
- Limits so one repo cannot hog the server: 60-second clone timeout; skip repos whose GitHub `size` is over 1 GB; at most 15 fightsable conflicts per match. Over the conflict limit, the bot says it is too many conflicts for one fight and stops.

## Components

Cargo workspace:

```
crates/core     conflict engine + fight simulation. Pure, no I/O, compiles to wasm32-unknown-unknown.
crates/cli      existing git-fight binary, using core.
crates/server   axum + tokio. HTTP, WebSockets, SQLite via sqlx, GitHub App via reqwest.
crates/wasm     wasm-bindgen bindings to core for the browser.
web/            TypeScript + Vite client with a canvas renderer.
```

One Dockerfile builds the WASM module, the static client, and the server, then runs a single process that serves the web client, the API, and the WebSockets.

Visual style follows the logo in `assets/`: background `#0A0A0B`, ours `#EEEEEA`, theirs `#FF4A1C`, dot-matrix titles. The canvas reuses the CLI ASCII sprites, drawn as monospace text so the browser match looks like the terminal.

### crates/core

No filesystem, no network, no `HashMap` iteration, no floating-point math. Integer or fixed-point only.

Responsibilities:

- Parse conflict-marker files into ordered hunks. Reconstruct a file given a side pick (`ours` / `theirs` / `both`) or leave a hunk unresolved. A file with no side picked must round-trip byte-for-byte (the existing CLI property).
- Simulate one round: two fighters, 30 ticks per second, punch / kick / block / special. Block lasts 15 ticks (half a second). Timer, HP, armor, special unlock are inputs to the sim, not things the sim fetches from git.
- `state_hash()` over every field that affects the next tick (positions, HP, stun, guard timers, RNG state, remaining time, input that just applied).
- A small PCG32 RNG lives in this crate. Match code seeds it. Dependency updates cannot change fight outcomes.

The same crate runs in the CLI, in the browser via WASM, and on the server. A golden test (fixed seed + scripted inputs) must produce the same final hash natively and under `wasm-bindgen-test` in Node.

### crates/cli

The existing `git-fight` binary. Mergetool install, conflicted-file discovery, git-blame fighter stats, terminal UI, `--no-fight` / `--pick`. It calls core for parse, sim, and resolve. CLI tests keep passing at every milestone.

Fighter stats stay CLI-local (they need git):

| Stat | Source |
|---|---|
| Name | Author of the latest commit on that side that touched the file |
| HP | Share of the file's lines in `git blame`, scaled to 80–120 |
| Armor | 10% less damage if that commit also changed a test file |
| Special | Unlocked if they committed on at least 3 different days in the past week |

Online matches compute the equivalent stats on the server (still not in core) and pass integers into the sim.

### crates/wasm

Thin `wasm-bindgen` wrappers: create a fight from a seed and fighter stats, submit a tick's inputs, step one tick, read render state, read `state_hash()`. No GitHub, no WebSockets.

### crates/server

Single axum process.

- `POST /webhooks/github` — raw body, signature first, then JSON.
- `GET /auth/github` and `GET /auth/github/callback` — GitHub App user authorization (web flow). Identity only; the GitHub user token is dropped after `GET /user`.
- `GET /ws` — WebSocket for a match room (`?match=<id>`). Role comes from the session cookie, not from the client.
- `GET /match/<id>`, `GET /replay/<id>` — SPA routes; the client loads WASM and either joins a room or plays a stored log.
- `GET /<owner>/<repo>/leaderboard` and `GET /badge/<owner>/<repo>/<user>` — Milestone 6.
- Static files from the Vite build for everything else.

Git work happens in a worker with timeouts (at most two clone/merge-tree/push jobs at once). The webhook returns 200 before that work. Clones are bare, partial (`--filter=blob:none`), and discarded when the match finishes or expires. The server process never `chdir`s into a user repo to run a build or a test.

`--lag-ms` on the server binary holds inbound and outbound WebSocket messages so lockstep can be tested under fake latency. `--instant` confirms ticks as fast as inputs arrive (browser lockstep tests, not production).

### web/

Vite + TypeScript. Canvas at 30 ticks per second. Modes:

- Demo (built-in example conflict, no repo).
- vs CPU, and 2 players on one keyboard, same keys as the CLI (`a s d f` / `j k l ;`).
- Online match (lockstep against the server).
- Replay (seed + input log, no inputs accepted).

Milestone 2 ships as a static site (GitHub Pages). From Milestone 3 the same client talks to the server.

## Flow: `/fight` comment to pushed branch

```
comment /fight
    → verify webhook signature
    → load PR, poll until mergeable is not null
    → refuse if already mergeable, too big, too many conflicts, or a match is already open
    → insert pending match (frozen SHAs) so later /fight and synchronize see it
    → bare partial clone + merge-tree
    → parse fightable hunks in core
    → name fighters (PR author vs base-side blame)
    → comment the link (or abort the row if clone/hunks fail)
    → OAuth login, lockstep rounds
    → if all rounds resolved and SHAs unchanged: plumbing commit, push new branch, comment
    → otherwise: comment why nothing was pushed
```

### 1. Webhook

`issue_comment` (created/edited) whose trimmed first line is `/fight`, on a pull request, not written by this app.

Read the raw body. Compute `HMAC-SHA256(webhook_secret, body)`. Constant-time compare to `X-Hub-Signature-256` (`sha256=` + hex). Missing or mismatch: `401`, do not parse. Then parse JSON.

Dedup on `X-GitHub-Delivery` (required after the signature check) and the SHA-256 of the raw body. GitHub HMAC has no timestamp: ignore `issue_comment` / `pull_request` events whose `updated_at` (or `created_at`) is missing, unparseable, or older than 24 hours, and prune delivery rows after that. Ignore bots, issue comments that are not on a PR, and comments that are not `/fight`.

`pull_request` events are used for `auto_challenge` (off unless `.github/git-fight.yml` contains `auto_challenge: true`) and to notice that a PR head or base moved during an open match.

### 2. Mergeability

`GET /repos/{owner}/{repo}/pulls/{number}` until `mergeable` is not `null` (exponential backoff, give up after a short cap). `true` means no conflicts: comment that there is nothing to fight. `false` continues.

### 3. Limits before clone

`GET /repos/{owner}/{repo}`: if `size` (kilobytes) is greater than `1048576` (1 GiB), comment that the repo is too large and stop.

If this PR already has a match in `pending` or `in_progress`, comment the existing link and stop.

### 4. Clone and conflicts

In-memory installation token. Bare partial clone of the installation repo, 60-second timeout, plumbing env vars above. Fetch the PR head and base SHAs.

```
git merge-tree --write-tree <base> <head>
```

Needs git 2.38+. Exit `0` is a clean merge (comment and stop). Exit `1` is conflicts: first line is the result tree OID; the **Conflicted file info** section lists paths. Do not walk the whole tree looking for markers.

For each conflicted path, `git cat-file blob <tree>:<path>` and parse with core. Skip non-regular files (symlinks `120000`, gitlinks `160000`), paths with `..` or a `.git` component, absolute paths, and blobs over a size cap. Binary / modify-delete / file-directory conflicts have no fightable hunks and are skipped.

Count fightable hunks. `0`: comment that the conflicts are not the kind git fight can play. `> 15`: comment that it is too many conflicts for one fight and stop. Otherwise one round per hunk, file path then hunk order, max 15. A pull request with two conflicted files is two rounds; the result branch applies each round's pick to that path.

Record `pr_head_sha` and `pr_base_sha` on the match **before** clone so a second `/fight` or a `synchronize` during git work still sees the open match. If clone or hunk collection fails, status becomes `aborted` and another `/fight` can start. Milestone 5 refuses to push if either SHA has moved.

### 5. Fighters

- **Ours** (left, PR / head): the pull request author.
- **Theirs** (right, base): `git blame` on the base-side lines of that hunk. The author of the base commit that last touched those lines.

Map commit author emails to GitHub logins through the commits API. Same person on both sides: a **mirror match** (that login may occupy both slots; 2-player keys like the CLI). No GitHub account for the other author: the CPU plays them under their name. If blamed authors differ across hunks, the right-side identity (and who may send right-side inputs) updates at round start.

### 6. Challenge comment

Insert the match (status `pending`, `expires_at` = now + 24 hours, random seed, stored hunks). Comment who is fighting, how many rounds, and `https://<public-host>/match/<id>`. If that row is aborted or expired before clone finishes (PR moved, 24h), do not post a fight link or a second “could not start”.

### 7. Play

Players log in with GitHub. Only the two fighter logins can take a slot; everyone else spectates. Seed, stats, and hunk metadata go to every client. Lockstep runs as in [Netcode](#netcode). Each round's winner is `ours`, `theirs`, or `draw`. A fighter who disconnects has 30 seconds to rejoin, then loses the **current** round. If a fighter never shows up, the match expires at 24 hours with no result and no push.

### 8. Result

After the last round (Milestone 5):

- Any draw, skip, or forfeit that left a hunk unresolved: push nothing; comment the unresolved paths.
- Re-fetch the PR. If `head` or `base` SHA changed: push nothing; say the fight was over outdated code and offer a rematch (`/fight` again). An aborted or expired match does not record leaderboard rounds or start another round.
- Otherwise build each resolved file in core from the winning side. `git hash-object -w` the blobs, a temporary index, `write-tree`, `commit-tree` with parents `(pr_head_sha, pr_base_sha)`. Commit message lists each round and who won it. Push **only** `refs/heads/git-fight/pr-<number>-<match-id>` (create, never `--force`).
- Comment: winner of each round, compare URL for the new branch, replay URL `/replay/<id>`.

Humans review and merge. The bot never opens or merges the PR.

## Netcode

Deterministic lockstep with input delay. Rollback (GGPO-style) is a stretch goal and is out of scope unless asked.

All peers, including the server, run the same core sim. **Only the server's result counts.** Clients render; they do not decide winners.

### Tick

30 ticks per second. Time is a `u32` tick index. Default `INPUT_DELAY = 3` ticks (100 ms). `--lag-ms` is extra one-way hold on the server, not a change to delay.

### Room

A match is one room: seed, two fighter slots, any number of spectators, current round, confirmed tick `n`.

On WebSocket connect the server reads the session, then sends:

```text
Hello { match_id, seed, input_delay, your_role, ours, theirs, round, confirmed_tick, you_are, path, hunk_index }
```

`your_role` is `ours`, `theirs`, `both` (mirror), or `spectator`. Spectators never have a fighter slot.

### Inputs

Fighters send only:

```text
Input { tick, buttons }
```

`tick` is the simulation tick the input is meant for. The client queues a local press for `local_display_tick + INPUT_DELAY` and also sends it. `buttons` is a small integer (idle / punch / kick / block / special), the same taps as the terminal: no key-release channel.

The server accepts an input only if:

- the session owns that slot,
- `tick` is in `(confirmed_tick, confirmed_tick + window]`,
- that tick has not already been confirmed.

Anything else is dropped. Spectators' `Input` messages are dropped.

### Confirm

The server confirms tick `n` when both slots have an input for `n`, or when the wait budget for `n` expires (late = idle). It then broadcasts:

```text
Tick { n, ours, theirs }
```

to every client, including spectators. Everyone, server included, applies those two inputs and steps one tick. After the step the server records `(n, ours, theirs)` in `match_inputs` and may send `Hash { n, state_hash }` so a desynced client can see it is wrong. After Hello, the server sends `Snapshot { round, seed, confirmed_tick, stats, ticks, path, hunk_index }` with the confirmed input log for the current round so a joiner or reconnect can catch up in one message. A desynced client does not get to overrule the server; it should resync from that snapshot (reconnect) or reload.

`--lag-ms` delays that broadcast (and inbound `Input`) by the requested milliseconds.

### Disconnect, join, expiry

- A fighter WebSocket drop starts a 30-second rejoin timer. Reconnect with the same login resumes the slot. If the timer fires, they lose the current round (`ours` or `theirs` KO). Later rounds can still be played if they return.
- Status stays `pending` until both human fighters have occupied their slots at least once (CPU slots count as present). That flip is a guarded `pending` → `in_progress` update, so a join cannot un-expire, un-abort, or un-finish a row. If that never happens, at `expires_at` (24 hours) the match becomes `expired`, the room closes, no result, no push. A match that did start (`in_progress`) also expires at `expires_at` if it is still open, so an abandoned fight cannot hold the one-open-match slot or keep a room spinning. That flip is one `UPDATE … RETURNING`; a fight that finishes on the deadline is not listed as expired and does not get an expired comment.
- Server restart: rooms rebuild from SQLite (`matches`, `match_inputs`, stored hunks). A pull-request match without hunks yet (clone still running, or clone failed and aborted) is not a room. Clients reconnect and receive `Hello` at the latest confirmed tick.
- A pending PR match with frozen SHAs and no hunks yet sends `Error { message: "preparing" }` and closes. The canvas keeps `preparing match…` (not `waiting for opponent`) and reconnects until hunks exist; the next socket then gets `Hello`. `expired`, `aborted`, `finished`, and `not found` are terminal. The live-room map drops a sender only if it is still that room; a closed sender is not reused. Joining a match that just ended gets `Error`, not a new 30 Hz room.

### Replay

A finished match already has `seed` and the full input log. `GET /replay/<id>` serves the client, which runs WASM locally feeding the log. No room, no inputs. `GET /ws?match=` on a finished match returns `Error { message: "finished" }` and does not spawn a room. After the last round the live room stops the 30 Hz clock and keeps existing sockets briefly so clients can read `End`; new sockets still get `Error`. Unfinished matches are not replayable. On restart, a live match whose current round already has a result in `match_inputs` runs that round's finish (next round or match over) instead of spawning a room that immediately exits.

## Database tables

SQLite via sqlx. Migrations run at server start on a single connection. No secrets, no installation tokens, no OAuth tokens. A crash mid-rebuild (`*_fk` / `*_nn` leftover, dest missing) is recovered on the next boot before `CREATE TABLE IF NOT EXISTS`.

### `matches`

| Column | Type | Notes |
|---|---|---|
| `id` | `TEXT` PK | URL- and ref-safe (lowercase hex). Used in `/match/<id>` and `git-fight/pr-<n>-<id>`. |
| `installation_id` | `INTEGER` | GitHub installation. |
| `owner`, `repo` | `TEXT` | |
| `pr_number` | `INTEGER` | |
| `pr_head_sha`, `pr_base_sha` | `TEXT` | Frozen at challenge time. |
| `seed` | `TEXT` | `u64` decimal. |
| `status` | `TEXT` | `pending` / `in_progress` / `finished` / `expired` / `aborted`. |
| `ours_login`, `theirs_login` | `TEXT` NULL | GitHub login, or NULL when that side is CPU-only. |
| `ours_name`, `theirs_name` | `TEXT` | Display names (CPU keeps the git author name). |
| `ours_kind`, `theirs_kind` | `TEXT` | `github` / `cpu` / `mirror`. |
| `input_delay_ticks` | `INTEGER` | Default 3. |
| `created_at`, `started_at`, `finished_at`, `expires_at` | `TEXT` | RFC 3339. `expires_at` = created + 24h. |
| `result_branch` | `TEXT` NULL | Set only after a successful create-only push. |
| `final_hash` | `TEXT` NULL | Server `state_hash` at match end. |
| `abort_reason` | `TEXT` NULL | `draw` / `forfeit` / `outdated` / `expired` / `too_many` / … |
| `challenge_comment_id` | `INTEGER` NULL | GitHub issue-comment id of the challenge. Outcome comments (result, draw, outdated, expired) edit this comment when set. |

### `match_hunks`

One row per round.

| Column | Type | Notes |
|---|---|---|
| `match_id` | `TEXT` | FK `matches.id`. |
| `round_index` | `INTEGER` | `0..n-1`. |
| `path` | `TEXT` | Repo-relative, already validated. |
| `hunk_index` | `INTEGER` | Nth fightable hunk in that file. |
| `ours_bytes`, `theirs_bytes`, `base_bytes` | `BLOB` | Hunk sides for later resolve. |
| `theirs_login`, `theirs_name` | `TEXT` NULL | Right-side identity for this round if it differs. |
| `winner` | `TEXT` NULL | `ours` / `theirs` / `draw` / `forfeit_ours` / `forfeit_theirs`. |

Primary key `(match_id, round_index)`.

### `match_inputs`

Replay log and lockstep resume.

| Column | Type | Notes |
|---|---|---|
| `match_id` | `TEXT` | |
| `round_index` | `INTEGER` | Round that produced this tick. |
| `tick` | `INTEGER` | Tick index within that round. |
| `ours` | `INTEGER` | Packed buttons. |
| `theirs` | `INTEGER` | Packed buttons. |

Primary key `(match_id, round_index, tick)`. Append-only (`INSERT OR IGNORE`; the first confirmed tick wins).

### `sessions`

| Column | Type | Notes |
|---|---|---|
| `id` | `TEXT` PK | Random. HttpOnly cookie, signed with `SESSION_KEY`. |
| `github_user_id` | `INTEGER` | |
| `github_login` | `TEXT` | |
| `created_at`, `expires_at` | `TEXT` | 14-day TTL. Expired rows are pruned. |

No GitHub access tokens here. Login exchanges the OAuth `code`, calls `GET /user`, stores id + login, discards the token. The `code` is length-capped before the token exchange.

### `player_stats`

Milestone 6. Per repo, per login: `wins`, `losses`, `kos`, `conflicts_caused`. `conflicts_caused` increments for the base-side blamed author of each fought hunk.

Primary key `(owner, repo, github_login)`.

### `webhook_deliveries`

| Column | Type | Notes |
|---|---|---|
| `delivery_id` | `TEXT` PK | `X-GitHub-Delivery`. |
| `received_at` | `TEXT` | |
| `body_hash` | `TEXT NOT NULL` unique | SHA-256 of the raw body. A captured payload replayed with a new delivery id is ignored. Rows without a hash are dropped on migrate. |

Installation tokens: process memory only, keyed by `installation_id`, cached until they expire, then dropped. OAuth `state` / PKCE verifier: memory or a short-lived signed cookie, not this database. GitHub `/fight` matches do not store local-demo share tokens; role is the session cookie only.

## GitHub App

Repository permissions (nothing else):

| Permission | Access | Why |
|---|---|---|
| Pull requests | Read & write | Read PR + files; write comments. |
| Contents | Read & write | Read blobs/commits; **create** `git-fight/pr-*` branches. |
| Metadata | Read | Required. |

Subscribe to events: **Issue comment**, **Pull request**.

User authorization is the App's web application flow so the match page knows the GitHub login. No extra account permissions. The user token is not kept.

`.github/git-fight.yml`:

```yaml
auto_challenge: true
```

Absent or `false`: only `/fight` starts a match.

## Threat model

### Forged webhooks

Anyone who can hit `POST /webhooks/github` can send a JSON body that looks like a `/fight` on a victim PR. If we trusted it, we would clone, comment, and eventually push `git-fight/*` with an installation token.

**Mitigation:** `X-Hub-Signature-256` is required. HMAC-SHA256 over the **raw** body, constant-time compare, **then** parse. No signature or mismatch → `401` and no JSON. The webhook secret never logs. Delivery IDs are required and recorded, and the body hash is unique, so a captured payload replayed later is ignored even if the delivery header is swapped. Events whose GitHub timestamps are older than 24 hours are ignored, and delivery rows older than that are pruned. Clone URLs come from the authenticated installation + `owner/repo` on the payload after signature check, not from an arbitrary URL field. Live App HTTP is only `https://api.github.com` and `https://github.com`.

### A non-fighter trying to play

A spectator (or a stranger who found the match URL) sends `Input` for a fighter slot, or spoofs a query param `role=ours`.

**Mitigation:** Role is assigned on the server from the session cookie + `matches.ours_login` / `theirs_login`. The cookie is random, HttpOnly, `SameSite=Lax`, integrity-protected with `SESSION_KEY`. Clients cannot pick a slot. Inputs from the wrong login or from spectators are dropped. CPU slots cannot be claimed. Mirror matches allow only that one login to send both sides. Share tokens (`?token=`) exist only for local anonymous matches; a GitHub fight stores none, and an empty token cannot claim a slot. Expired session rows are pruned.

### Hostile repos

A repo can be huge, contain symlink farms, `.git` path tricks, enormous blobs, odd encodings in filenames, or executable hooks.

**Mitigation:**

- Skip when GitHub `size` > 1 GiB; clone timeout 60 seconds; `--filter=blob:none`; bare repo; no checkout of a worktree used as a cwd for user code.
- `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, `core.hooksPath=/dev/null`. Git is invoked with argument lists, never a shell string built from paths.
- Paths from merge-tree (`-z`) must be relative, with no `..` or `.git` component. Only regular-file modes. Do not follow symlinks. Cap blob bytes. Cap 15 hunks.
- Never `cargo test`, never a repo `Dockerfile`, never `git submodule update`, never a post-checkout hook.

### Comment spam

`/fight` in a loop, or a bot that replies to itself, burns clone quota and floods the PR.

**Mitigation:** Ignore this app's own comments and `sender.type == Bot` unless we have a specific allow-list (we do not). One active `pending`/`in_progress` match per PR; extra `/fight` gets the existing link. Per-PR and per-installation rate limits on starting matches. `auto_challenge` defaults off. Webhook delivery dedup. Clone/size/hunk limits still apply.

## Milestone map

| Milestone | What lands | Stop after |
|---|---|---|
| 0 | This file and `SETUP.md` | No code |
| 1 | `crates/core` + CLI on core; PCG32; `state_hash`; native + WASM golden tests in CI | `cargo fmt`, `clippy -D warnings`, `cargo test`, web tests |
| 2 | `web/` + `crates/wasm`; vs CPU / 2P / demo; Pages workflow; Playwright KO smoke | same gates |
| 3 | Rooms, lockstep, `--lag-ms`, disconnect/expiry, replays; two headless WS clients agree with server hash | same gates |
| 4 | GitHub App: `/fight`, merge-tree, fighter mapping, OAuth; wiremock + signature tests | same gates |
| 5 | Resolve, plumbing commit, create-only push, draw/outdated comments; e2e vs wiremock + local remote | same gates |
| 6 | Leaderboard, shields-style badge, README Online section | same gates |

After each milestone: run the gates, summarize, list what to try by hand, and wait for go.
