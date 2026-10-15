import { defineConfig } from '@playwright/test'

export default defineConfig({
  testDir: './tests/browser',
  workers: 1,
  timeout: 45000,
  use: { baseURL: 'http://localhost:31819', viewport: { width: 1280, height: 900 }, trace: 'retain-on-failure' },
  webServer: {
    command: 'cargo run --manifest-path ../Cargo.toml --example web_test_server',
    url: 'http://localhost:31819/health',
    timeout: 300000,
    reuseExistingServer: !process.env.CI,
  },
})
