#!/bin/sh
# GitHub App process for Playwright. Waits for web/dist from the local-demo server.
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
for _ in $(seq 1 180); do
  if [ -f "$root/web/dist/index.html" ]; then
    break
  fi
  sleep 1
done
if [ ! -f "$root/web/dist/index.html" ]; then
  echo "web/dist/index.html missing" >&2
  exit 1
fi
export GITHUB_APP_ID=1
# Hosting injects the PEM as a single line with \n; the binary expands it.
GITHUB_APP_PRIVATE_KEY=$(awk '{printf "%s\\n", $0}' "$root/crates/server/tests/fixtures/app_key.txt")
export GITHUB_APP_PRIVATE_KEY
export GITHUB_CLIENT_ID=Iv1.playwright
export GITHUB_CLIENT_SECRET=client-secret-for-tests
export GITHUB_WEBHOOK_SECRET=webhook-secret-for-tests
export SESSION_KEY=session-key-session-key-session!
export GIT_FIGHT_PUBLIC_URL=http://127.0.0.1:18081
unset GITHUB_API_URL || true
unset GITHUB_OAUTH_URL || true
exec cargo run -q --manifest-path "$root/Cargo.toml" -p git-fight-server -- \
  --bind 127.0.0.1:18081 \
  --static "$root/web/dist" \
  --db "sqlite://$root/target/playwright-gh.db" \
  --instant
