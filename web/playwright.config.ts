import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  retries: 0,
  use: {
    baseURL: "http://127.0.0.1:18080",
    trace: "on-first-retry",
  },
  webServer: {
    command:
      "npm run build && cargo run -q --manifest-path ../Cargo.toml -p git-fight-server -- --bind 127.0.0.1:18080 --static dist --db sqlite://../target/playwright.db --instant",
    url: "http://127.0.0.1:18080/health",
    reuseExistingServer: !process.env.CI,
    timeout: 180_000,
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
