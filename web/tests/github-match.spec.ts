import { createHmac } from "node:crypto";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

const SESSION_KEY = "session-key-session-key-session!";
const DB = join(dirname(fileURLToPath(import.meta.url)), "../../target/playwright.db");

function signSid(id: string): string {
  const mac = createHmac("sha256", SESSION_KEY).update(id).digest("hex");
  return `${id}.${mac}`;
}

function sidCookie(sid: string) {
  return {
    name: "git_fight_sid",
    value: signSid(sid),
    url: "http://127.0.0.1:18080/",
  };
}

async function createMatch(request: APIRequestContext): Promise<string> {
  const created = await request.post("/api/matches", { data: { seed: 9 } });
  expect(created.ok(), await created.text()).toBeTruthy();
  const body = (await created.json()) as { id: string };
  return body.id;
}

function attachGithubMatch(
  matchId: string,
  theirs: { kind: "cpu" | "github"; login: string | null },
): void {
  const theirsLoginSql = theirs.login === null ? "NULL" : `'${theirs.login}'`;
  const hunkLoginSql = theirs.login === null ? "NULL" : `'${theirs.login}'`;
  const sql = `
UPDATE matches SET
  ours_login = 'alice', theirs_login = ${theirsLoginSql},
  ours_name = 'alice', theirs_name = 'bob',
  ours_kind = 'github', theirs_kind = '${theirs.kind}',
  owner = 'acme', repo = 'box', pr_number = 0
WHERE id = '${matchId}';
DELETE FROM match_inputs WHERE match_id = '${matchId}';
DELETE FROM match_hunks WHERE match_id = '${matchId}';
INSERT INTO match_hunks (
  match_id, round_index, path, hunk_index, ours_bytes, theirs_bytes, base_bytes,
  theirs_login, theirs_name, ours_hp, ours_armor, ours_special, theirs_hp, theirs_armor, theirs_special
) VALUES
  ('${matchId}', 0, 'a.rs', 0, X'61', X'62', X'63', ${hunkLoginSql}, 'bob', 100, 0, 0, 100, 0, 0),
  ('${matchId}', 1, 'b.rs', 0, X'61', X'62', X'63', ${hunkLoginSql}, 'bob', 100, 0, 0, 100, 0, 0);
DELETE FROM sessions WHERE id IN ('sid-alice', 'sid-bob');
INSERT INTO sessions (id, github_user_id, github_login, created_at, expires_at) VALUES
  ('sid-alice', 1, 'alice', '2020-01-01T00:00:00+00:00', '2099-01-01T00:00:00+00:00'),
  ('sid-bob', 2, 'bob', '2020-01-01T00:00:00+00:00', '2099-01-01T00:00:00+00:00');
`;
  execFileSync("sqlite3", [DB, sql], { stdio: "pipe" });
}

async function mashUntil(pages: Page[], pred: () => Promise<boolean>, ms: number): Promise<void> {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline && !(await pred())) {
    for (const page of pages) {
      await page.keyboard.press("a");
    }
    await pages[0]?.waitForTimeout(30);
  }
}

async function playTwoRounds(page: Page, matchId: string): Promise<void> {
  const resolved = page.getByTestId("resolved");
  await mashUntil(
    [page],
    async () => {
      const text = (await resolved.textContent()) ?? "";
      return text.includes("1/2") || text.includes("/replay/");
    },
    20_000,
  );
  if ((await resolved.textContent())?.includes("/replay/") !== true) {
    await expect(resolved).toContainText("1/2");
    await mashUntil([page], async () => (await resolved.textContent())?.includes("/replay/") === true, 20_000);
  }
  await expect(resolved).toContainText(`/replay/${matchId}`);
  await expect(page.getByTestId("ko")).toBeVisible();
  await expect(page.getByTestId("ko")).toHaveText(/KO|DRAW/);
}

test("github session match plays two CPU rounds in the browser", async ({ context, page, request }) => {
  const matchId = await createMatch(request);
  attachGithubMatch(matchId, { kind: "cpu", login: null });
  await context.addCookies([sidCookie("sid-alice")]);
  await page.goto(`/match/${matchId}`);
  const stage = page.getByTestId("stage");
  await expect(stage).toBeVisible();
  await expect(stage).toHaveAttribute("data-path", /a\.rs|b\.rs/, { timeout: 10_000 });
  await expect(page.getByTestId("wait")).not.toContainText(/spectating/i);

  await stage.click();
  await playTwoRounds(page, matchId);

  const replay = await page.request.get(`/api/replays/${matchId}`);
  expect(replay.ok()).toBeTruthy();
  const replayBody = (await replay.json()) as { rounds?: Array<{ path?: string; hunk_index?: number }> };
  expect(replayBody.rounds).toHaveLength(2);
  expect(replayBody.rounds?.map((r) => r.path)).toEqual(["a.rs", "b.rs"]);
  expect(replayBody.rounds?.map((r) => r.hunk_index)).toEqual([0, 0]);

  await page.goto(`/replay/${matchId}`);
  await expect(page.getByTestId("stage")).toHaveAttribute("data-path", "a.rs", { timeout: 10_000 });
});

test("logged-out visitor gets a GitHub login link instead of a fighter slot", async ({ page, request }) => {
  const matchId = await createMatch(request);
  attachGithubMatch(matchId, { kind: "cpu", login: null });
  await page.goto(`/match/${matchId}`);
  const login = page.getByTestId("github-login");
  await expect(login).toBeVisible({ timeout: 10_000 });
  await expect(login).toHaveAttribute("href", `/auth/github?return=/match/${matchId}`);
  await expect(page.getByTestId("wait")).toContainText(/log in with GitHub/i);
});

test("two GitHub sessions play two rounds in the browser", async ({ browser, request }) => {
  const matchId = await createMatch(request);
  attachGithubMatch(matchId, { kind: "github", login: "bob" });

  const aliceCtx = await browser.newContext();
  const bobCtx = await browser.newContext();
  await aliceCtx.addCookies([sidCookie("sid-alice")]);
  await bobCtx.addCookies([sidCookie("sid-bob")]);
  const alice = await aliceCtx.newPage();
  const bob = await bobCtx.newPage();

  await alice.goto(`/match/${matchId}`);
  await bob.goto(`/match/${matchId}`);
  await expect(alice.getByTestId("wait")).not.toContainText(/spectating/i);
  await expect(bob.getByTestId("wait")).not.toContainText(/spectating/i);
  await expect(alice.getByTestId("stage")).toHaveAttribute("data-path", /a\.rs|b\.rs/, { timeout: 10_000 });

  await alice.getByTestId("stage").click();
  await bob.getByTestId("stage").click();
  const resolved = alice.getByTestId("resolved");
  await mashUntil(
    [alice, bob],
    async () => {
      const text = (await resolved.textContent()) ?? "";
      return text.includes("1/2") || text.includes("/replay/");
    },
    20_000,
  );
  if ((await resolved.textContent())?.includes("/replay/") !== true) {
    await expect(resolved).toContainText("1/2");
    await mashUntil(
      [alice, bob],
      async () => (await resolved.textContent())?.includes("/replay/") === true,
      20_000,
    );
  }
  await expect(resolved).toContainText(`/replay/${matchId}`);
  await expect(alice.getByTestId("ko")).toBeVisible();
  await expect(bob.getByTestId("ko")).toBeVisible();
  await expect(alice.getByTestId("ko")).toHaveText(/KO|DRAW/);
  await expect(bob.getByTestId("ko")).toHaveText(/KO|DRAW/);

  const replay = await alice.request.get(`/api/replays/${matchId}`);
  expect(replay.ok()).toBeTruthy();
  const replayBody = (await replay.json()) as { rounds?: unknown[] };
  expect(replayBody.rounds).toHaveLength(2);

  await aliceCtx.close();
  await bobCtx.close();
});
