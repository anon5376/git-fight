import { createHmac } from "node:crypto";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Page } from "@playwright/test";

const SESSION_KEY = "session-key-session-key-session!";
const DB = join(dirname(fileURLToPath(import.meta.url)), "../../target/playwright.db");

function signSid(id: string): string {
  const mac = createHmac("sha256", SESSION_KEY).update(id).digest("hex");
  return `${id}.${mac}`;
}

function attachGithubCpuSides(matchId: string): void {
  const sql = `
UPDATE matches SET
  ours_login = 'alice', theirs_login = NULL,
  ours_name = 'alice', theirs_name = 'bob',
  ours_kind = 'github', theirs_kind = 'cpu',
  owner = 'acme', repo = 'box', pr_number = 0
WHERE id = '${matchId}';
DELETE FROM match_inputs WHERE match_id = '${matchId}';
DELETE FROM match_hunks WHERE match_id = '${matchId}';
INSERT INTO match_hunks (
  match_id, round_index, path, hunk_index, ours_bytes, theirs_bytes, base_bytes,
  theirs_login, theirs_name, ours_hp, ours_armor, ours_special, theirs_hp, theirs_armor, theirs_special
) VALUES
  ('${matchId}', 0, 'a.rs', 0, X'61', X'62', X'63', NULL, 'bob', 100, 0, 0, 100, 0, 0),
  ('${matchId}', 1, 'b.rs', 0, X'61', X'62', X'63', NULL, 'bob', 100, 0, 0, 100, 0, 0);
DELETE FROM sessions WHERE id = 'sid-alice';
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

test("github session match plays two CPU rounds in the browser", async ({ context, page, request }) => {
  const created = await request.post("/api/matches", { data: { seed: 9 } });
  expect(created.ok(), await created.text()).toBeTruthy();
  const body = (await created.json()) as { id: string };
  const matchId = body.id;
  attachGithubCpuSides(matchId);
  await context.addCookies([
    {
      name: "git_fight_sid",
      value: signSid("sid-alice"),
      url: "http://127.0.0.1:18080/",
    },
  ]);
  await page.goto(`/match/${matchId}`);
  const stage = page.getByTestId("stage");
  const wait = page.getByTestId("wait");
  const resolved = page.getByTestId("resolved");
  await expect(stage).toBeVisible();
  await expect(wait).not.toContainText(/spectating/i);

  await stage.click();
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
