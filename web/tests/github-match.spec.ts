import { createHmac } from "node:crypto";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Page } from "@playwright/test";

const SESSION_KEY = "session-key-session-key-session!";
const DB = join(dirname(fileURLToPath(import.meta.url)), "../../target/playwright.db");
const MATCH_ID = "cafe0008cafe0008cafe0008cafe0008";

function signSid(id: string): string {
  const mac = createHmac("sha256", SESSION_KEY).update(id).digest("hex");
  return `${id}.${mac}`;
}

function seedGithubCpuMatch(): void {
  const sql = `
DELETE FROM match_inputs WHERE match_id = '${MATCH_ID}';
DELETE FROM match_hunks WHERE match_id = '${MATCH_ID}';
DELETE FROM matches WHERE id = '${MATCH_ID}';
DELETE FROM sessions WHERE id = 'sid-alice';
INSERT INTO matches (
  id, installation_id, owner, repo, pr_number, pr_head_sha, pr_base_sha,
  seed, status, ours_login, theirs_login, ours_name, theirs_name,
  ours_kind, theirs_kind, ours_token, theirs_token, input_delay_ticks,
  created_at, expires_at
) VALUES (
  '${MATCH_ID}', 1, 'acme', 'box', 0, '', '',
  '9', 'pending', 'alice', NULL, 'alice', 'bob',
  'github', 'cpu', 'o', 't', 3,
  '2020-01-01T00:00:00+00:00', '2099-01-01T00:00:00+00:00'
);
INSERT INTO match_hunks (
  match_id, round_index, path, hunk_index, ours_bytes, theirs_bytes, base_bytes,
  theirs_login, theirs_name, ours_hp, ours_armor, ours_special, theirs_hp, theirs_armor, theirs_special
) VALUES
  ('${MATCH_ID}', 0, 'a.rs', 0, X'61', X'62', X'63', NULL, 'bob', 100, 0, 0, 100, 0, 0),
  ('${MATCH_ID}', 1, 'b.rs', 0, X'61', X'62', X'63', NULL, 'bob', 100, 0, 0, 100, 0, 0);
INSERT INTO sessions (id, github_user_id, github_login, created_at, expires_at)
VALUES ('sid-alice', 1, 'alice', '2020-01-01T00:00:00+00:00', '2099-01-01T00:00:00+00:00');
`;
  execFileSync("sqlite3", [DB, sql], { stdio: "pipe" });
}

async function mashUntil(page: Page, pred: () => Promise<boolean>, ms: number): Promise<void> {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline && !(await pred())) {
    await page.keyboard.press("a");
    await page.waitForTimeout(30);
  }
}

test("github session match plays two CPU rounds in the browser", async ({ context, page }) => {
  seedGithubCpuMatch();
  await context.addCookies([
    {
      name: "git_fight_sid",
      value: signSid("sid-alice"),
      url: "http://127.0.0.1:18080/",
    },
  ]);
  await page.goto(`/match/${MATCH_ID}`);
  const stage = page.getByTestId("stage");
  const wait = page.getByTestId("wait");
  const resolved = page.getByTestId("resolved");
  await expect(stage).toBeVisible();
  await expect(stage).toHaveAttribute("data-round", /1\/2/, { timeout: 10_000 });
  await expect(wait).not.toContainText(/spectating/i);

  await stage.click();
  await mashUntil(page, async () => (await resolved.textContent())?.includes("1/2") === true, 20_000);
  await expect(resolved).toContainText("1/2");

  await expect(stage).toHaveAttribute("data-round", /2\/2/, { timeout: 10_000 });

  await mashUntil(page, async () => (await resolved.textContent())?.includes("/replay/") === true, 20_000);
  await expect(resolved).toContainText(`/replay/${MATCH_ID}`);
  await expect(page.getByTestId("ko")).toBeVisible();
  await expect(page.getByTestId("ko")).toHaveText(/KO|DRAW/);

  const replay = await page.request.get(`/api/replays/${MATCH_ID}`);
  expect(replay.ok()).toBeTruthy();
  const body = (await replay.json()) as { rounds?: unknown[] };
  expect(body.rounds).toHaveLength(2);
});
