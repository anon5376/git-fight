import { expect, test } from "@playwright/test";

test("hosted lockstep match reaches KO in two browsers", async ({ browser }) => {
  const oursCtx = await browser.newContext();
  const theirsCtx = await browser.newContext();
  const specCtx = await browser.newContext();
  const ours = await oursCtx.newPage();
  const theirs = await theirsCtx.newPage();
  const spec = await specCtx.newPage();

  await ours.goto("/");
  await expect(ours.getByTestId("menu")).toBeVisible();
  await ours.getByTestId("host").click();
  await expect(ours).toHaveURL(/\/match\/[0-9a-f]+/i);
  await expect(ours.getByTestId("share")).toContainText("opponent link:");
  const share = await ours.getByTestId("share").textContent();
  const opponent = share?.replace("opponent link: ", "").trim() ?? "";
  expect(opponent).toMatch(/\/match\/.+\?token=/);

  await theirs.goto(opponent);
  await expect(theirs.getByTestId("stage")).toBeVisible();

  const matchUrl = ours.url().split("?")[0];
  await spec.goto(matchUrl);
  await expect(spec.getByTestId("wait")).toContainText(/spectating/i);

  const stage = ours.getByTestId("stage");
  await stage.click();
  const ko = ours.getByTestId("ko");
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline && !(await ko.isVisible())) {
    await ours.keyboard.press("a");
    await ours.waitForTimeout(40);
  }

  await expect(stage).toHaveAttribute("data-theirs-hp", "0");
  await expect(ko).toBeVisible();
  await expect(ko).toHaveText("KO");
  await expect(theirs.getByTestId("ko")).toBeVisible();

  await oursCtx.close();
  await theirsCtx.close();
  await specCtx.close();
});
