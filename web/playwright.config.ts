import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  workers: 1,
  retries: 0,
  use: {
    baseURL: "http://127.0.0.1:18080",
    trace: "on-first-retry",
  },
  webServer: [
    {
      command:
        "env -u GITHUB_APP_ID -u GITHUB_APP_PRIVATE_KEY -u GITHUB_CLIENT_ID -u GITHUB_CLIENT_SECRET npm run build && cargo run -q --manifest-path ../Cargo.toml -p git-fight-server -- --bind 127.0.0.1:18080 --static dist --db sqlite://../target/playwright.db --instant",
      url: "http://127.0.0.1:18080/health",
      reuseExistingServer: !process.env.CI,
      timeout: 180_000,
      env: {
        ...process.env,
        SESSION_KEY: "session-key-session-key-session!",
      },
    },
    {
      command: "sh ../scripts/playwright-app-env.sh",
      url: "http://127.0.0.1:18081/health",
      reuseExistingServer: !process.env.CI,
      timeout: 180_000,
    },
  ],
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
      testIgnore: /github-app\.spec\.ts/,
    },
    {
      name: "github-app",
      use: { ...devices["Desktop Chrome"], baseURL: "http://127.0.0.1:18081" },
      testMatch: /github-app\.spec\.ts/,
    },
  ],
});

