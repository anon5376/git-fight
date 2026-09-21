import { expect, test } from "@playwright/test";

test("cpu match reaches the KO screen", async ({ page }) => {
  await page.goto("/?smoke=1");
  await expect(page.getByTestId("menu")).toBeVisible();
  await page.getByTestId("cpu").click();
  await page.getByTestId("play-ours").click();
  const stage = page.getByTestId("stage");
  await expect(stage).toBeVisible();
  await stage.click();

  const ko = page.getByTestId("ko");
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline && !(await ko.isVisible())) {
    await page.keyboard.press("a");
    await page.waitForTimeout(50);
  }

  await expect(stage).toHaveAttribute("data-theirs-hp", "0");
  await expect(ko).toBeVisible();
  await expect(ko).toHaveText("KO");
});
