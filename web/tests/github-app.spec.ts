import { createHmac, randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Page } from "@playwright/test";

const SESSION_KEY = "session-key-session-key-session!";
const DB = join(dirname(fileURLToPath(import.meta.url)), "../../target/playwright-gh.db");
const SHA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BASE = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

function signSid(id: string): string {
  const mac = createHmac("sha256", SESSION_KEY).update(id).digest("hex");
  return `${id}.${mac}`;
}

function insertFightMatch(matchId: string): void {
  const sql = `
UPDATE matches SET status = 'aborted', abort_reason = 'test'
  WHERE owner = 'acme' AND repo = 'box' AND pr_number = 7
    AND status IN ('pending', 'in_progress');
INSERT INTO matches (
  id, installation_id, owner, repo, pr_number, pr_head_sha, pr_base_sha,
  seed, status, ours_login, theirs_login, ours_name, theirs_name,
  ours_kind, theirs_kind, ours_token, theirs_token, input_delay_ticks,
  created_at, expires_at
) VALUES (
  -- installation_id stays NULL so finish does not call live api.github.com
  '${matchId}', NULL, 'acme', 'box', 7, '${SHA}', '${BASE}',
  '9', 'pending', 'alice', NULL, 'alice', 'bob',
  'github', 'cpu', '', '', 3,
  '2020-01-01T00:00:00+00:00', '2099-01-01T00:00:00+00:00'
);
INSERT INTO match_hunks (
  match_id, round_index, path, hunk_index, ours_bytes, theirs_bytes, base_bytes,
  theirs_login, theirs_name, ours_hp, ours_armor, ours_special, theirs_hp, theirs_armor, theirs_special
) VALUES
  ('${matchId}', 0, 'a.rs', 0, X'61', X'62', X'63', NULL, 'bob', 100, 0, 0, 100, 0, 0),
  ('${matchId}', 1, 'b.rs', 0, X'61', X'62', X'63', NULL, 'bob', 100, 0, 0, 100, 0, 0);
DELETE FROM sessions WHERE id = 'sid-alice';
INSERT INTO sessions (id, github_user_id, github_login, created_at, expires_at) VALUES
  ('sid-alice', 1, 'alice', '2020-01-01T00:00:00+00:00', '2099-01-01T00:00:00+00:00');
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

test("GitHub App host does not mint local tokens", async ({ request }) => {
  const res = await request.post("/api/matches", { data: {} });
  expect(res.status()).toBe(404);
  const text = await res.text();
  expect(text).not.toContain("ours_token");
  expect(text).not.toContain("theirs_token");
});

test("GitHub App session match plays two CPU rounds without share tokens", async ({
  context,
  page,
}) => {
  const matchId = randomUUID().replace(/-/g, "");
  insertFightMatch(matchId);
  await context.addCookies([
    {
      name: "git_fight_sid",
      value: signSid("sid-alice"),
      url: "http://127.0.0.1:18081/",
    },
  ]);
  await page.goto(`/match/${matchId}`);
  const stage = page.getByTestId("stage");
  await expect(stage).toBeVisible();
  await expect(stage).toHaveAttribute("data-path", /a\.rs|b\.rs/, { timeout: 10_000 });
  await expect(stage).toHaveAttribute("data-role", "ours");
  await expect(stage).toHaveAttribute("data-you-are", "alice");
  await expect(page.getByTestId("wait")).not.toContainText(/spectating/i);

  await stage.click();
  const resolved = page.getByTestId("resolved");
  await mashUntil(
    page,
    async () => {
      const text = (await resolved.textContent()) ?? "";
      return text.includes("1/2") || text.includes("/replay/");
    },
    20_000,
  );
  if ((await resolved.textContent())?.includes("/replay/") !== true) {
    await expect(resolved).toContainText("1/2");
    await mashUntil(page, async () => (await resolved.textContent())?.includes("/replay/") === true, 20_000);
  }
  await expect(resolved).toContainText(`/replay/${matchId}`);
  await expect(page.getByTestId("ko")).toBeVisible();
  await expect(page.getByTestId("ko")).toHaveText(/KO|DRAW/);

  const replay = await page.request.get(`/api/replays/${matchId}`);
  expect(replay.ok()).toBeTruthy();
  const replayBody = (await replay.json()) as { rounds?: unknown[] };
  expect(replayBody.rounds).toHaveLength(2);
});
