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

type GithubHunk = { path: string; login: string | null; name: string };

function sqlStr(value: string | null): string {
  return value === null ? "NULL" : `'${value}'`;
}

function attachGithubMatch(
  matchId: string,
  theirs: { kind: "cpu" | "github" | "mirror"; login: string | null },
  hunks?: GithubHunk[],
  prNumber = 0,
): void {
  const oursLogin = theirs.kind === "mirror" ? (theirs.login ?? "alice") : "alice";
  const oursKind = theirs.kind === "mirror" ? "mirror" : "github";
  const rounds = hunks ?? [
    { path: "a.rs", login: theirs.login, name: theirs.login ?? "bob" },
    { path: "b.rs", login: theirs.login, name: theirs.login ?? "bob" },
  ];
  const hunkValues = rounds
    .map(
      (h, i) =>
        `('${matchId}', ${i}, '${h.path}', 0, X'61', X'62', X'63', ${sqlStr(h.login)}, '${h.name}', 100, 0, 0, 100, 0, 0)`,
    )
    .join(",\n  ");
  const sql = `
UPDATE matches SET status = 'aborted', abort_reason = 'test'
  WHERE owner = 'acme' AND repo = 'box' AND pr_number = ${prNumber}
    AND status IN ('pending', 'in_progress') AND id != '${matchId}'
    AND ${prNumber} > 0;
UPDATE matches SET
  ours_login = ${sqlStr(oursLogin)}, theirs_login = ${sqlStr(theirs.login)},
  ours_name = '${oursLogin}', theirs_name = '${rounds[0]?.name ?? "bob"}',
  ours_kind = '${oursKind}', theirs_kind = '${theirs.kind}',
  owner = 'acme', repo = 'box', pr_number = ${prNumber}
WHERE id = '${matchId}';
DELETE FROM match_inputs WHERE match_id = '${matchId}';
DELETE FROM match_hunks WHERE match_id = '${matchId}';
INSERT INTO match_hunks (
  match_id, round_index, path, hunk_index, ours_bytes, theirs_bytes, base_bytes,
  theirs_login, theirs_name, ours_hp, ours_armor, ours_special, theirs_hp, theirs_armor, theirs_special
) VALUES
  ${hunkValues};
DELETE FROM sessions WHERE id IN ('sid-alice', 'sid-bob', 'sid-carol');
INSERT INTO sessions (id, github_user_id, github_login, created_at, expires_at) VALUES
  ('sid-alice', 1, 'alice', '2020-01-01T00:00:00+00:00', '2099-01-01T00:00:00+00:00'),
  ('sid-bob', 2, 'bob', '2020-01-01T00:00:00+00:00', '2099-01-01T00:00:00+00:00'),
  ('sid-carol', 3, 'carol', '2020-01-01T00:00:00+00:00', '2099-01-01T00:00:00+00:00');
`;
  execFileSync("sqlite3", [DB, sql], { stdio: "pipe" });
}

async function mashUntil(
  pages: Page[],
  pred: () => Promise<boolean>,
  ms: number,
  keys: string[] = ["a"],
): Promise<void> {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline && !(await pred())) {
    for (const page of pages) {
      for (const key of keys) {
        await page.keyboard.press(key);
      }
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
  await expect(stage).toHaveAttribute("data-role", "ours");
  await expect(stage).toHaveAttribute("data-you-are", "alice");
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
  await expect(page.getByTestId("stage")).toHaveAttribute("data-role", "spectator", { timeout: 10_000 });
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
  await expect(alice.getByTestId("stage")).toHaveAttribute("data-role", "ours");
  await expect(alice.getByTestId("stage")).toHaveAttribute("data-you-are", "alice");
  await expect(bob.getByTestId("stage")).toHaveAttribute("data-role", "theirs");
  await expect(bob.getByTestId("stage")).toHaveAttribute("data-you-are", "bob");

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

test("logged-in teammate spectates without a GitHub login prompt", async ({ browser, request }) => {
  const matchId = await createMatch(request);
  attachGithubMatch(matchId, { kind: "github", login: "bob" });

  const aliceCtx = await browser.newContext();
  const carolCtx = await browser.newContext();
  await aliceCtx.addCookies([sidCookie("sid-alice")]);
  await carolCtx.addCookies([sidCookie("sid-carol")]);
  const alice = await aliceCtx.newPage();
  const carol = await carolCtx.newPage();

  await alice.goto(`/match/${matchId}`);
  await carol.goto(`/match/${matchId}`);
  await expect(carol.getByTestId("stage")).toHaveAttribute("data-role", "spectator", { timeout: 10_000 });
  await expect(carol.getByTestId("stage")).toHaveAttribute("data-you-are", "carol");
  await expect(carol.getByTestId("wait")).toContainText(/spectating as carol/i);
  await expect(carol.getByTestId("github-login")).toHaveCount(0);
  await expect(alice.getByTestId("wait")).not.toContainText(/spectating/i);
  await expect(alice.getByTestId("stage")).toHaveAttribute("data-role", "ours");

  await aliceCtx.close();
  await carolCtx.close();
});

test("theirs slot follows the blamed author each round", async ({ browser, request }) => {
  const matchId = await createMatch(request);
  attachGithubMatch(matchId, { kind: "github", login: "bob" }, [
    { path: "a.rs", login: "bob", name: "bob" },
    { path: "b.rs", login: "carol", name: "carol" },
  ]);

  const aliceCtx = await browser.newContext();
  const bobCtx = await browser.newContext();
  const carolCtx = await browser.newContext();
  await aliceCtx.addCookies([sidCookie("sid-alice")]);
  await bobCtx.addCookies([sidCookie("sid-bob")]);
  await carolCtx.addCookies([sidCookie("sid-carol")]);
  const alice = await aliceCtx.newPage();
  const bob = await bobCtx.newPage();
  const carol = await carolCtx.newPage();

  await alice.goto(`/match/${matchId}`);
  await carol.goto(`/match/${matchId}`);
  await expect(carol.getByTestId("wait")).toContainText(/spectating as carol/i, { timeout: 10_000 });
  await expect(carol.getByTestId("github-login")).toHaveCount(0);
  await expect(carol.getByTestId("stage")).toHaveAttribute("data-role", "spectator");
  await expect(alice.getByTestId("stage")).toHaveAttribute("data-path", "a.rs");
  await expect(alice.getByTestId("stage")).toHaveAttribute("data-theirs-name", "bob");

  await bob.goto(`/match/${matchId}`);
  await expect(bob.getByTestId("stage")).toHaveAttribute("data-role", "theirs", { timeout: 10_000 });
  await expect(bob.getByTestId("stage")).toHaveAttribute("data-you-are", "bob");
  await expect(bob.getByTestId("github-login")).toHaveCount(0);

  await alice.getByTestId("stage").click();
  await bob.getByTestId("stage").click();
  await carol.getByTestId("stage").click();
  await mashUntil(
    [alice, bob, carol],
    async () => (await carol.getByTestId("stage").getAttribute("data-role")) === "theirs",
    25_000,
  );
  await expect(carol.getByTestId("stage")).toHaveAttribute("data-role", "theirs");
  await expect(carol.getByTestId("stage")).toHaveAttribute("data-you-are", "carol");
  await expect(carol.getByTestId("stage")).toHaveAttribute("data-path", "b.rs");
  await expect(carol.getByTestId("stage")).toHaveAttribute("data-theirs-name", "carol");
  await expect(bob.getByTestId("stage")).toHaveAttribute("data-role", "spectator");
  await expect(alice.getByTestId("stage")).toHaveAttribute("data-role", "ours");
  await expect(carol.getByTestId("github-login")).toHaveCount(0);

  const resolved = alice.getByTestId("resolved");
  await mashUntil(
    [alice, bob, carol],
    async () => ((await resolved.textContent()) ?? "").includes("/replay/"),
    20_000,
  );
  await expect(resolved).toContainText(`/replay/${matchId}`);

  await aliceCtx.close();
  await bobCtx.close();
  await carolCtx.close();
});

test("mirror GitHub session plays both slots with 2P keys", async ({ context, page, request }) => {
  const matchId = await createMatch(request);
  attachGithubMatch(matchId, { kind: "mirror", login: "alice" }, [
    { path: "a.rs", login: "alice", name: "alice" },
    { path: "b.rs", login: "alice", name: "alice" },
  ]);
  await context.addCookies([sidCookie("sid-alice")]);
  await page.goto(`/match/${matchId}`);
  const stage = page.getByTestId("stage");
  await expect(stage).toHaveAttribute("data-role", "both", { timeout: 10_000 });
  await expect(stage).toHaveAttribute("data-you-are", "alice");
  await expect(stage).toHaveAttribute("data-ours-name", "alice");
  await expect(stage).toHaveAttribute("data-theirs-name", "alice");
  await expect(page.getByTestId("wait")).toContainText(/both sides/i);
  await expect(page.getByTestId("wait")).not.toContainText(/spectating/i);
  await expect(page.getByTestId("github-login")).toHaveCount(0);

  await stage.click();
  const resolved = page.getByTestId("resolved");
  await mashUntil(
    [page],
    async () => ((await resolved.textContent()) ?? "").includes("/replay/"),
    25_000,
    ["a", "j"],
  );
  await expect(resolved).toContainText(`/replay/${matchId}`);

  const replay = await page.request.get(`/api/replays/${matchId}`);
  expect(replay.ok()).toBeTruthy();
  const replayBody = (await replay.json()) as { rounds?: Array<{ ticks?: number[][] }> };
  expect(replayBody.rounds).toHaveLength(2);
  const ticks = replayBody.rounds?.flatMap((r) => r.ticks ?? []) ?? [];
  expect(ticks.some((t) => t[0] === 1)).toBeTruthy();
  expect(ticks.some((t) => t[1] === 1)).toBeTruthy();
});

test("preparing PR match stays on preparing until hunks exist", async ({
  context,
  page,
  request,
}) => {
  const matchId = await createMatch(request);
  execFileSync(
    "sqlite3",
    [
      DB,
      `UPDATE matches SET owner = 'prep', repo = '${matchId}', pr_number = 1, status = 'pending' WHERE id = '${matchId}';
       DELETE FROM match_hunks WHERE match_id = '${matchId}';`,
    ],
    { stdio: "pipe" },
  );
  await context.addCookies([sidCookie("sid-alice")]);
  await page.goto(`/match/${matchId}`);
  const wait = page.getByTestId("wait");
  await expect(wait).toContainText(/preparing match/i, { timeout: 10_000 });
  await page.waitForTimeout(900);
  await expect(wait).toContainText(/preparing match/i);
  await expect(wait).not.toContainText(/reconnecting/i);
  await expect(wait).not.toContainText(/reloading/i);
  await expect(wait).not.toContainText(/waiting for opponent/i);

  attachGithubMatch(matchId, { kind: "cpu", login: null }, undefined, 1);
  const stage = page.getByTestId("stage");
  await expect(stage).toHaveAttribute("data-role", "ours", { timeout: 10_000 });
  await expect(stage).toHaveAttribute("data-you-are", "alice");
  await expect(stage).toHaveAttribute("data-path", /a\.rs|b\.rs/);
  await expect(wait).not.toContainText(/preparing match/i);
  await expect(wait).not.toContainText(/reconnecting/i);
});

test("expired match shows expiry and does not reconnect", async ({ page, request }) => {
  const matchId = await createMatch(request);
  execFileSync(
    "sqlite3",
    [
      DB,
      `UPDATE matches SET status = 'expired', abort_reason = 'expired' WHERE id = '${matchId}';`,
    ],
    { stdio: "pipe" },
  );
  await page.goto(`/match/${matchId}`);
  await expect(page.getByTestId("wait")).toContainText(/this match expired/i, { timeout: 10_000 });
  await page.waitForTimeout(900);
  await expect(page.getByTestId("wait")).toContainText(/this match expired/i);
  await expect(page.getByTestId("wait")).not.toContainText(/reconnecting/i);
  await expect(page.getByTestId("wait")).not.toContainText(/reloading/i);
});

test("outdated match shows rematch and does not reconnect", async ({ page, request }) => {
  const matchId = await createMatch(request);
  execFileSync(
    "sqlite3",
    [
      DB,
      `UPDATE matches SET status = 'aborted', abort_reason = 'outdated' WHERE id = '${matchId}';`,
    ],
    { stdio: "pipe" },
  );
  await page.goto(`/match/${matchId}`);
  await expect(page.getByTestId("wait")).toContainText(/\/fight/i, { timeout: 10_000 });
  await expect(page.getByTestId("wait")).toContainText(/pr moved/i);
  await page.waitForTimeout(900);
  await expect(page.getByTestId("wait")).toContainText(/\/fight/i);
  await expect(page.getByTestId("wait")).not.toContainText(/reconnecting/i);
  await expect(page.getByTestId("wait")).not.toContainText(/reloading/i);
});

test("repo leaderboard and badge render in the browser", async ({ page }) => {
  execFileSync(
    "sqlite3",
    [
      DB,
      `DELETE FROM player_stats WHERE owner = 'acme' AND repo = 'box-lb';
INSERT INTO player_stats (owner, repo, github_login, wins, losses, kos, conflicts_caused)
VALUES
  ('acme', 'box-lb', 'alice', 3, 1, 2, 0),
  ('acme', 'box-lb', 'bob', 1, 3, 0, 4);`,
    ],
    { stdio: "pipe" },
  );

  await page.goto("/acme/box-lb/leaderboard");
  await expect(page.locator(".tag")).toHaveText("acme/box-lb leaderboard");
  await expect(page.locator("table")).toContainText("alice");
  await expect(page.locator("table")).toContainText("bob");
  const aliceRow = page.locator("tbody tr").filter({ hasText: "alice" });
  await expect(aliceRow).toContainText("3");
  await expect(aliceRow).toContainText("2");
  await expect(page.getByTestId("stage")).toHaveCount(0);
  await expect(page.getByTestId("wait")).toHaveCount(0);

  const badge = await page.goto("/badge/acme/box-lb/alice");
  expect(badge?.ok()).toBeTruthy();
  expect(badge?.headers()["content-type"] ?? "").toMatch(/image\/svg\+xml/);
  const svg = (await badge?.text()) ?? "";
  expect(svg).toContain("3 wins");
  expect(svg).toContain("#FF4A1C");
  expect(svg).toContain("#0A0A0B");
});
