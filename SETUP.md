# Setup

Hand steps on github.com to run git fight online. The app does not exist until you register it. Secrets stay in environment variables. Do not commit them, paste them into issues, or put them in SQLite.

Production host below is `https://<your-host>`. Local server listens on `http://127.0.0.1:8080`. Replace both.

## 1. Open the GitHub App form

1. Sign in to GitHub.
2. Profile photo → **Settings**.
3. Sidebar → **Developer settings** → **GitHub Apps**.
4. **New GitHub App**.

Direct link: [https://github.com/settings/apps/new](https://github.com/settings/apps/new).

To own the app as an organization: organization **Settings** → **Developer settings** → **GitHub Apps** → **New GitHub App**.

## 2. Identity fields

| Field | What to enter |
|---|---|
| **GitHub App name** | Unique on GitHub, ≤ 34 characters. Example: `git fight`. |
| **Description** | Optional. Shown at install time. |
| **Homepage URL** | Production: `https://<your-host>`. Until then, this repository's URL is fine. |

## 3. Callback URL (user login)

Players log in with the App's user authorization web flow so the server can tell a fighter from a spectator.

**Callback URL:**

- Local: `http://127.0.0.1:8080/auth/github/callback`
- Production: `https://<your-host>/auth/github/callback`

GitHub allows up to 10 callback URLs. Add both. The server will send `redirect_uri` so the right one is used.

Leave **Expire user authorization tokens** checked.

Leave **Request user authorization (OAuth) during installation** **unchecked**. Login starts from the match page (`GET /auth/github`), not from install. Checking this box would always redirect to the first callback URL and would block a separate setup URL.

Leave **Enable Device Flow** unchecked.

**Setup URL** (optional): `https://<your-host>` or the repository URL. This is where GitHub sends people after they install the app.

## 4. Webhook URL and secret

Keep **Webhook Active** checked.

**Webhook URL:**

- Local development: a smee.io channel (see [Forward webhooks to localhost](#8-forward-webhooks-to-localhost-smeeio)).
- Production: `https://<your-host>/webhooks/github`

**Webhook secret:** a long random string. Generate one and keep it:

```bash
openssl rand -hex 32
```

Put the same value in GitHub's **Webhook secret** field and in `GITHUB_WEBHOOK_SECRET`. GitHub sends `X-Hub-Signature-256` using this secret. The server rejects unsigned or badly signed bodies before it parses JSON.

Leave **SSL verification** enabled. smee.io is HTTPS, so local development still uses SSL from GitHub to smee.

## 5. Permissions

Repository permissions **only**. Everything else stays **No access**.

| Permission | Access |
|---|---|
| **Contents** | **Read and write** (clone, read blobs, push **new** `git-fight/pr-*` branches) |
| **Metadata** | **Read-only** (required) |
| **Pull requests** | **Read and write** (read the PR, write comments) |

Do not enable Issues, Checks, Actions, Administration, Workflows, or account permissions (email, profile). Login uses `GET /user` on a user-to-server token from this App; that does not need extra account permissions.

## 6. Events

Under **Subscribe to events**, check only:

- **Issue comment** (`/fight` on the PR)
- **Pull request** (`auto_challenge`, and detecting a head/base that moved)

## 7. Install where, then create

**Where can this GitHub App be installed?**

- **Only on this account** while you are testing.
- **Any account** if other people will install it.

Click **Create GitHub App**.

On the app's settings page, copy:

- **App ID** → `GITHUB_APP_ID`
- **Client ID** → `GITHUB_CLIENT_ID`

Under **Client secrets**, click **Generate a new client secret**. Copy it once → `GITHUB_CLIENT_SECRET`. GitHub will not show it again.

Under **Private keys**, click **Generate a private key**. A `.pem` file downloads. GitHub keeps only the public half. Put the **PEM text** in `GITHUB_APP_PRIVATE_KEY` (the `-----BEGIN …` block, including newlines). Do not commit the `.pem`. You can keep up to 25 keys if you rotate.

Generate a session key (never from GitHub):

```bash
openssl rand -hex 32
```

That is `SESSION_KEY`.

## 8. Forward webhooks to localhost (smee.io)

GitHub cannot POST to `127.0.0.1`. smee.io is a **development** proxy: GitHub sends HTTPS to a public channel, the smee client forwards to your machine. Channels are unauthenticated. Anyone with the channel URL can read payloads. **Do not use smee in production.**

1. Open [https://smee.io](https://smee.io).
2. Click **Start a new channel**.
3. Copy the **Webhook Proxy URL** (`https://smee.io/<id>`).
4. GitHub App settings → **Webhook URL** → paste that URL → **Save changes**.
5. Install the client once:

```bash
npm install --global smee-client
```

6. Start the git fight server locally (port `8080` in this doc).
7. In a second terminal:

```bash
smee --url https://smee.io/<id> --path /webhooks/github --port 8080
```

You should see:

```text
Forwarding https://smee.io/<id> to http://127.0.0.1:8080/webhooks/github
Connected https://smee.io/<id>
```

Keep both processes running. Trigger a `/fight` comment (or **Redeliver** from the App's **Advanced** webhook deliveries). smee should print `POST http://127.0.0.1:8080/webhooks/github` with a 2xx. The smee channel page should show the payload.

To stop forwarding: `Ctrl+C` in the smee terminal.

When you deploy, change the App's **Webhook URL** to `https://<your-host>/webhooks/github` and stop smee.

## 9. Install the app on a repo

GitHub App settings → **Install App** → choose your account → **Only select repositories** → pick a throwaway repo with a conflicted PR.

The install creates an installation ID. You do not need to put it in the environment; webhooks include it and the server exchanges it for an installation token in memory.

## 10. Environment variables

No `.env` in git. Export these in the shell, your process manager, or the container runtime.

| Variable | From |
|---|---|
| `GITHUB_APP_ID` | App settings, **App ID** |
| `GITHUB_CLIENT_ID` | App settings, **Client ID** |
| `GITHUB_CLIENT_SECRET` | **Generate a new client secret** |
| `GITHUB_APP_PRIVATE_KEY` | Contents of the downloaded `.pem` |
| `GITHUB_WEBHOOK_SECRET` | The string you put in **Webhook secret** |
| `SESSION_KEY` | `openssl rand -hex 32` |
| `GIT_FIGHT_PUBLIC_URL` | Required when GitHub App credentials are set. Local `http://127.0.0.1:8080` or `https://<your-host>` (OAuth `redirect_uri` and match links). Wildcard binds (`0.0.0.0`) are rejected — the Docker image binds `0.0.0.0:8080` and must set this to the public host. |
| `DATABASE_URL` | Example: `sqlite://data/git-fight.db` |

If any of the four App credentials is set, all four plus webhook secret, session key, and public URL are required or the process exits. It does not fall back to a local demo.

Optional later: `--lag-ms` on the server binary (not a secret).

Check git is **2.38+** (`git merge-tree --write-tree` exists):

```bash
git version
```

## 11. Local vs production checklist

**Local**

- Webhook URL = smee channel; `smee` forwarding to port 8080.
- Callback URL includes `http://127.0.0.1:8080/auth/github/callback`.
- `GIT_FIGHT_PUBLIC_URL=http://127.0.0.1:8080`.
- App installed on a test repo.

**Production**

- Webhook URL = `https://<your-host>/webhooks/github`.
- Callback URL includes `https://<your-host>/auth/github/callback`.
- `GIT_FIGHT_PUBLIC_URL=https://<your-host>`.
- TLS on `<your-host>`. SSL verification stays on.
- Same secrets via the host's secret store, injected as env vars. Still not on disk in the git repo, still not in SQLite.

## 12. What you should see after a dry run

Once Milestone 4 exists, this loop is enough to check the App:

1. Open a PR that cannot merge.
2. Comment `/fight`.
3. GitHub → App → **Advanced** → the `issue_comment` delivery is green.
4. The bot comments who fights, round count, and a `/match/<id>` link.
5. Opening that link asks for GitHub login, then the canvas.

If the delivery is red, the server was down, smee was down, or the path/port does not match. If the server logs a signature error, `GITHUB_WEBHOOK_SECRET` does not match the App's Webhook secret (after a change, GitHub only signs new deliveries with the new secret).
