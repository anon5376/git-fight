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

Map commit author emails to GitHub logins through the commits API. Same person on both sides: a **mirror match** (that login may occupy both slots; 2-player keys like the CLI). No GitHub account for the other author: the CPU plays them under their name. A blamed or stored login that is not a GitHub name is the same as no account — dropped, not kept as a human slot that nobody can occupy. If the blamed commit already has an `author.login` that is not a GitHub name, do not fall back to `?author=email` (that list is any recent commit with the address, not this SHA). A transient commits-API failure is not “no account”: the challenge does not start (quiet abort) so rematch `/fight` can look the login up again. If blamed authors differ across hunks, the right-side identity (and who may send right-side inputs) updates at round start.

### 6. Challenge comment

Insert the match (status `pending`, `expires_at` = now + 24 hours, random seed, stored hunks). Comment who is fighting, how many rounds, and `https://<public-host>/match/<id>`. If that row is aborted or expired before clone finishes (PR moved, 24h), do not post a fight link or a second “could not start”.

### 7. Play

Players log in with GitHub. Only the two fighter logins can take a slot; everyone else spectates. Seed, stats, and hunk metadata go to every client. Lockstep runs as in [Netcode](#netcode). Each round's winner is `ours`, `theirs`, or `draw`. A fighter who disconnects has 30 seconds to rejoin, then loses the **current** round. If a fighter never shows up, the match expires at 24 hours with no result and no push. That includes a later-round blamed author who has not occupied the right-side slot yet: the 30-second clock does not start until they have been seen, so a multi-author PR does not auto-forfeit the next conflict 30 seconds after the previous KO. An aborted or expired row stops confirming ticks on the next sim step, even if Shutdown is still queued. `--instant` re-checks before each step in a burst. A round that already ended in the sim does not write End or a hunk winner after the row is closed. That open-status read is `status` only. If the winner write fails while the row is still open and that hunk exists with no winner yet, the room retries instead of broadcasting End or starting the next conflict. A match with no hunk row (local demo) still ends. A disconnect forfeit keeps the `forfeit_*` tag across that retry so a busy SQLite write cannot become a KO pick. An open-status read error is not a closed match: the room retries instead of expiring or skipping a live fight. A busy `pending` → `in_progress` write does not expire the row; join and the clock retry the guarded start. After clone, a busy open-status read still posts the challenge if hunks exist. The last-round `finished` write retries the same way so a won fight cannot sit `in_progress` until the 24-hour expiry buries it. A fully scored open match is not expired: boot and the 5s expirer mark it finished and publish. Leaderboard writes are claim-once per round (`stats_recorded`) so that path can record a missing round without double-counting. The claim and the `player_stats` deltas commit together so a failed leaderboard write can retry. A KO is stored on the hunk (`is_ko`) so a resume after `finished` still records that KO once, never after abort or expiry. Boot migrate must not mark unrecorded winners as recorded. Hello and Snapshot clip the hunk path so a 4096-byte name cannot bloat the room. The canvas drops held buttons when Hello starts the next round so a leftover key from the KO cannot confirm after `next_tick` resets. A non-final End also advances the Input `round` and resets the send cursor so a late or dropped next-round Hello cannot leave those buttons tagged for the finished conflict (`--instant` does not idle-confirm). A non-final End is broadcast only after a successful open-status read so a busy SQLite check cannot leave clients on the next Input `round` while the room stays on this conflict. The last-round `End` (`match_over`) and expiry `Error` are queued if the outbound channel is full — there is no follow-up Hello to recover them. The next-round Hello is queued if the outbound channel is full. Spectator `Error { busy }` retries like `preparing` (no desync reload) so a teammate watching is not kicked off when Join cannot enter the room queue. Confirmed ticks are written to `match_inputs` before the sim steps or the Tick is broadcast, so a busy SQLite write cannot leave lockstep ahead of the replay log. A transient blamed-author HTTP miss aborts the pending row instead of storing CPU / `theirs_login = NULL`, so a GitHub blip cannot lock the right-side slot to the computer. GitHub-match Input must carry the Hello `round`; omitted `round` is dropped so a leftover KO press cannot confirm after `next_tick` resets. A later-round blamed author cannot put Input on the room queue until their conflict is the current round.

### 8. Result

After the last round (Milestone 5):

- Any draw, skip, or forfeit that left a hunk unresolved: push nothing; comment the unresolved paths.
- Re-fetch the PR. If `head` or `base` SHA changed: push nothing; say the fight was over outdated code and offer a rematch (`/fight` again). A re-fetched SHA that is not 40-char hex is a transient pull failure (retry), not a permanent outdated skip. Webhook `synchronize` ignores those junk SHAs too, so they cannot abort a live fight. An aborted or expired match does not record leaderboard rounds or start another round. Hunk winners are write-once and only while the row is still `pending` or `in_progress`.
- Otherwise build each resolved file in core from the winning side. `git hash-object -w` the blobs, a temporary index, `write-tree`, `commit-tree` with parents `(pr_head_sha, pr_base_sha)`. Commit message lists each round and who won it. Push **only** `refs/heads/git-fight/pr-<number>-<match-id>` (create, never `--force`). If that ref already points at this match's commit (author `git-fight`, those parents, message `git fight match <id>`), that is success — crash recovery, not overwrite. Resolve ignores a `hunk_index` outside `0..15` so a hostile row cannot allocate on publish.
- `result_branch` and skip `abort_reason` are write-once and mutually exclusive. A later skip cannot clobber a stored branch.
- Comment: winner of each round, compare URL for the new branch, replay URL `/replay/<id>`.
- Server restart retries finished GitHub matches that still have no `result_branch` and no skip reason. Transient clone, token, pull, or push failures do not write a skip reason, so boot and the expirer can try the create-only push again. Decision skips (draw, forfeit, outdated, exists, no conflicts, gone PR) still write once and comment. Round winners are write-once on an open match; `stats_recorded` is write-once on pending, in_progress, or finished (never aborted or expired), so a resume, scored-open finish, or crash after `finished` records a missing leaderboard round once and does not start another round after SHA-drift/expiry.

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

`your_role` is `ours`, `theirs`, `both` (mirror), or `spectator`. Spectators never have a fighter slot. Outbound Tick/Hash/End/Hello (including join Hello/Snapshot and closed-room Error) are non-blocking so a client who stops reading cannot stall confirm; they resync from Snapshot on reconnect. Join Hello/Snapshot, the next-round Hello, last-round End, and expiry Error are queued if the outbound channel is full.

### Inputs

Fighters send only:

```text
Input { tick, buttons, round }
```

`tick` is the simulation tick the input is meant for. The client queues a local press for `local_display_tick + INPUT_DELAY` and also sends it. `buttons` is a small integer (idle / punch / kick / block / special), the same taps as the terminal: no key-release channel. `round` is the Hello round; an in-flight Input from a finished round (early KO, then the next conflict) is dropped so those buttons cannot steer the next sim.

The server accepts an input only if:

- the session owns that slot,
- on a GitHub match, `round` matches the current Hello round (omitted is dropped so leftover KO buttons cannot steer the next conflict),
- on a local demo, `round` is omitted or matches the current round,
- `tick` is in `(confirmed_tick, confirmed_tick + window]`,
- that tick has not already been confirmed.

Anything else is dropped. Spectators' `Input` messages are dropped before the room queue. That includes a later-round blamed author during an earlier conflict, and a previous-round theirs after their hunk is scored: only the current-round fighter logins (ours + this hunk's theirs) may enqueue. Spectator `Join` is `try_send` (full = reject) so a connect flood cannot fill that queue either. The current-round theirs is re-read on each Input so a later-round author who connected as a spectator can play when their conflict starts, without filling the queue before then.

### Confirm

The server confirms tick `n` when both slots have an input for `n`, or when the wait budget for `n` expires (late = idle). It records `(n, ours, theirs)` in `match_inputs` first. Only after that row is durable (insert or already present) does it broadcast:

```text
Tick { n, ours, theirs }
```

to every client, including spectators. Everyone, server included, applies those two inputs and steps one tick. A failed write does not consume the pending buttons or advance `confirmed_tick`; the next clock retries. A closed match that cannot store the tick expires instead of confirming. The server may send `Hash { n, state_hash }` so a desynced client can see it is wrong. After Hello, the server sends `Snapshot { round, seed, confirmed_tick, stats, ticks, path, hunk_index }` with the confirmed input log for the current round so a joiner or reconnect can catch up in one message. A desynced client does not get to overrule the server; it should resync from that snapshot (reconnect) or reload.

`--lag-ms` delays that broadcast (and inbound `Input`) by the requested milliseconds.

### Disconnect, join, expiry

- A fighter WebSocket drop starts a 30-second rejoin timer **after that login has occupied the slot**. Reconnect with the same login resumes the slot. If the timer fires, they lose the current round (`ours` or `theirs` KO). Later rounds can still be played if they return. A blamed author who has not been seen on the current slot is treated as never-showed-up (wait until `expires_at`), not as a disconnect. The clock does not start merely because the match is already `in_progress`.
- Status stays `pending` until both human fighters have occupied their slots at least once (CPU slots count as present). That flip is a guarded `pending` → `in_progress` update, so a join cannot un-expire, un-abort, or un-finish a row. If that never happens, at `expires_at` (24 hours) the match becomes `expired`, the room closes, no result, no push. A match that did start (`in_progress`) also expires at `expires_at` if it is still open, so an abandoned fight cannot hold the one-open-match slot or keep a room spinning. That flip is one `UPDATE … RETURNING`; a fight that finishes on the deadline is not listed as expired and does not get an expired comment. An open match whose every hunk already has a winner is finished and published instead of expired, so a crash after the last write cannot bury a won fight at 24 hours. The live room is closed before the expiry comment HTTP so GitHub latency cannot keep confirming ticks on an expired row. SHA-drift abort does the same: close the room, then comment. Closing a room does not hold the live-room map while Shutdown is delivered.
- Server restart: rooms rebuild from SQLite (`matches`, `match_inputs`, stored hunks). A pull-request match without hunks yet (clone still running, or clone failed and aborted) is not a room. A failed `match_inputs` read does not start confirm from an empty log — the room retries the load and withholds Hello until the durable ticks are replayed, so resume cannot diverge from the stored log. Clients reconnect and receive `Hello` at the latest confirmed tick. Finished GitHub matches with no result branch and no skip reason are published again (create-only; an existing git-fight ref for this match counts as done). A scored-open finish that cannot hash the last round (busy input read) retries instead of writing a fake `final_hash`.
- A pending PR match with frozen SHAs and no hunks yet sends `Error { message: "preparing" }` and closes. The canvas keeps `preparing match…` (not `waiting for opponent`) and reconnects until hunks exist; the next socket then gets `Hello`. Spectator `Join` that cannot enter the room queue sends `Error { message: "busy" }` and closes. The canvas retries that the same way (`match busy — retrying`), without burning reconnects or reloading, so a teammate watching during a full queue is not kicked into a desync reload. `expired`, `aborted`, `finished`, and `not found` are terminal. The live-room map drops a sender only if it is still that room; a closed sender is not reused. Joining a match that just ended gets `Error`, not a new 30 Hz room.

### Replay

A finished match already has `seed` and the full input log. `GET /replay/<id>` serves the client, which runs WASM locally feeding the log. No room, no inputs. `GET /ws?match=` on a finished match returns `Error { message: "finished" }` and does not spawn a room. After the last round the live room stops the 30 Hz clock and keeps existing sockets briefly so clients can read `End`; new sockets still get `Error`. Unfinished matches are not replayable. On restart, a live match whose current round already has a result in `match_inputs` runs that round's finish (next round or match over) instead of spawning a room that immediately exits. If every hunk already has a winner, the match is marked finished and published; it is not replayed from round 0. The 5s expirer does the same finish+publish for a scored open row that has no live room, and `expire_pending` skips those rows. Boot and that tick also record any pending/in_progress/finished hunk whose `stats_recorded` is still 0.

## Database tables

SQLite via sqlx. Migrations run at server start on a single connection. No secrets, no installation tokens, no OAuth tokens. A crash mid-rebuild (`*_fk` / `*_nn` leftover, dest missing) is recovered on the next boot before `CREATE TABLE IF NOT EXISTS`.

### `matches`

| Column | Type | Notes |
|---|---|---|
| `id` | `TEXT` PK | URL- and ref-safe (lowercase hex). Used in `/match/<id>` and `git-fight/pr-<n>-<id>`. |
| `installation_id` | `INTEGER` | GitHub installation. |
| `owner`, `repo` | `TEXT` | Stored lowercase. GitHub owner/repo are case-insensitive. The one-open-match index compares them without case. |
| `pr_number` | `INTEGER` | |
| `pr_head_sha`, `pr_base_sha` | `TEXT` | Frozen at challenge time. |
| `seed` | `TEXT` | `u64` decimal. |
| `status` | `TEXT` | `pending` / `in_progress` / `finished` / `expired` / `aborted`. |
| `ours_login`, `theirs_login` | `TEXT` NULL | GitHub login stored lowercase, or NULL when that side is CPU-only. Compared case-insensitively. |
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
| `hunk_index` | `INTEGER` | Nth fightable hunk in that file (`0..15`). Out of range is not stored and is ignored at resolve. |
| `ours_bytes`, `theirs_bytes`, `base_bytes` | `BLOB` | Unused at rest. Resolve rebuilds from merge-tree + picks so a hostile 1 MiB hunk cannot sit in SQLite. |
| `theirs_login`, `theirs_name` | `TEXT` NULL | Right-side identity for this round if it differs. |
| `winner` | `TEXT` NULL | `ours` / `theirs` / `draw` / `forfeit_ours` / `forfeit_theirs`. |
| `is_ko` | `INTEGER` | 1 when that round ended in a KO. Recovery uses this; forfeit, draw, and timeout stay 0. |
| `stats_recorded` | `INTEGER` | Write-once while pending, in_progress, or finished. Claim and `player_stats` commit together. |

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

Primary key `(match_id, round_index, tick)`. Append-only (`INSERT OR IGNORE`; the first confirmed tick wins). Inserts only while the match is `pending` or `in_progress`. The room writes the row before stepping or broadcasting `Tick`; an already-present row counts as durable.

### `sessions`

| Column | Type | Notes |
|---|---|---|
| `id` | `TEXT` PK | Random. HttpOnly cookie, signed with `SESSION_KEY`. |
| `github_user_id` | `INTEGER` | |
| `github_login` | `TEXT` | Stored lowercase. GitHub logins are case-insensitive. |
| `created_at`, `expires_at` | `TEXT` | 14-day TTL. Expired rows are pruned. |

No GitHub access tokens here. Login exchanges the OAuth `code`, calls `GET /user`, stores id + login, discards the token. The `code` is length-capped before the token exchange.

### `player_stats`

Milestone 6. Per repo, per login: `wins`, `losses`, `kos`, `conflicts_caused`. `conflicts_caused` increments for the base-side blamed author of each fought hunk.

Primary key `(owner, repo, github_login)`. Owner, repo, and login are stored lowercase so `Acme/Box` / `Alice` and `acme/box` / `alice` are one row.

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

**Mitigation:** `X-Hub-Signature-256` is required. HMAC-SHA256 over the **raw** body, constant-time compare, **then** parse. HMAC construction does not fall back to a zero key. No signature or mismatch → `401` and no JSON. The webhook secret never logs. Delivery IDs are required and recorded, and the body hash is unique, so a captured payload replayed later is ignored even if the delivery header is swapped. Events whose GitHub timestamps are older than 24 hours are ignored, and delivery rows older than that are pruned. Clone URLs come from the authenticated installation + `owner/repo` on the payload after signature check, not from an arbitrary URL field. Live App HTTP is only `https://api.github.com` and `https://github.com`.

### A non-fighter trying to play

A spectator (or a stranger who found the match URL) sends `Input` for a fighter slot, or spoofs a query param `role=ours`.

**Mitigation:** Role is assigned on the server from the session cookie + `matches.ours_login` / `theirs_login`. GitHub logins, owners, and repos are compared case-insensitively and stored lowercase so a fighter cannot be locked out of their slot or split on the leaderboard, and a second `/fight` cannot bypass the one-open-match slot by changing case. A blamed or session login that is not a GitHub name is dropped (CPU / no session), not stored as a slot nobody can claim. The cookie is random, HttpOnly, `SameSite=Lax`, integrity-protected with `SESSION_KEY`. Clients cannot pick a slot. Inputs from the wrong login or from spectators are dropped **before** they enter the room event queue, so a spectator flood cannot fill the 512-slot channel and stall fighter confirm or Leave. Spectator `Join` is `try_send` (full = reject); spectator `Leave` does not block the read task. Fighter `Input` is `try_send` (late = idle). CPU slots cannot be claimed. Mirror matches allow only that one login to send both sides. Share tokens (`?token=`) exist only for local anonymous matches; a GitHub fight stores none, and an empty token cannot claim a slot. Expired session rows are pruned. Room broadcasts do not wait on a full client buffer, so a silent spectator cannot freeze lockstep.

### Hostile repos

A repo can be huge, contain symlink farms, `.git` path tricks, enormous blobs, odd encodings in filenames, or executable hooks.

**Mitigation:**

- Skip when GitHub `size` > 1 GiB; clone timeout 60 seconds; `--filter=blob:none`; bare repo; no checkout of a worktree used as a cwd for user code.
- `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, `core.hooksPath=/dev/null`. Git is invoked with argument lists, never a shell string built from paths. Object SHAs are passed after `--` except where git would then treat the SHA as a path (`diff-tree` tree-ish, `log` revision-range, `rev-parse --verify`). `git log --author=` takes the revision, then `--`.
- Paths from merge-tree (`-z`) must be relative, with no `..` or `.git` component, no control characters, no option-like (`-`) names, and no component over 255 bytes or path over 4096 bytes. Only regular-file modes. Do not follow symlinks. Cap blob bytes (skip that path; other fightable files still start a match). Do not store those blobs in SQLite, and drop them from RAM after fighter stats so they do not sit across blamed-author HTTP. Hello, Snapshot, result comments, and the result commit message clip paths so a 4096-byte name cannot fill a GitHub comment or the room. GitHub comments and the result commit message keep only a safe charset for paths and author names (no `_` / `*` / `[]()`), so a hostile name cannot inject markdown links, images, emphasis, or @mentions into the bot's comment. Hunk rows reject a path that is not fightable and a `round_index` / `hunk_index` outside `0..15`; round winners are an allow-list. Cap git stdout/stderr so a huge blob or merge-tree list cannot fill RAM. Blame locates a hunk with a bounded search so a 1 MiB conflict cannot be quadratic against the file. Git author names and emails are length-capped so a hostile commit cannot bloat Hello or challenge comments. `git log --author` treats the name as a literal (regex metacharacters escaped). Cap 15 hunks.
- Never `cargo test`, never a repo `Dockerfile`, never `git submodule update`, never a post-checkout hook.

### Comment spam

`/fight` in a loop, or a bot that replies to itself, burns clone quota and floods the PR.

**Mitigation:** Ignore this app's own comments and `sender.type == Bot` unless we have a specific allow-list (we do not). One active `pending`/`in_progress` match per PR; extra `/fight` gets the existing link. Owner/repo casing cannot open a second slot or reset the per-PR rate limit. Per-PR and per-installation rate limits on starting matches. `auto_challenge` defaults off. Webhook delivery dedup. Clone/size/hunk limits still apply.

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
