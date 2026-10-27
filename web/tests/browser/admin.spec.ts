import { readFile } from 'node:fs/promises'

import { type Page, expect, test } from '@playwright/test'

async function login(page: Page) {
  await page.goto('/')
  await expect(page.getByLabel('验证方式')).toHaveValue('passkey')
  await page.getByLabel('验证方式').selectOption('password')
  await page.getByLabel('Master Password', { exact: true }).fill('browser test')
  await page.getByRole('button', { name: '验证身份', exact: true }).click()
  await expect(page.getByRole('heading', { name: '签发访问令牌' })).toBeVisible()
}

test('login defaults to passkey and keeps password and TOTP available', async ({ page }) => {
  await page.goto('/')
  const method = page.getByLabel('验证方式')
  await expect(method).toHaveValue('passkey')
  await expect(method.locator('option').first()).toHaveAttribute('value', 'passkey')
  await expect(page.getByRole('button', { name: '使用通行密钥验证' })).toBeVisible()
  await expect(page.locator('#login-value')).toHaveCount(0)
  await page.screenshot({ path: 'test-results/desktop-login.png', fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/mobile-login.png', fullPage: true })
  await method.selectOption('totp')
  await expect(page.getByLabel('6 位动态码')).toHaveAttribute('inputmode', 'numeric')
  await method.selectOption('password')
  await expect(page.getByLabel('Master Password', { exact: true })).toBeVisible()
  await page.reload()
  await expect(page.getByLabel('验证方式')).toHaveValue('passkey')
})

test('passkey default keeps the localhost link when opened through an IP address', async ({ page }) => {
  await page.goto('http://127.0.0.1:31819/')
  await expect(page.getByLabel('验证方式')).toHaveValue('passkey')
  await expect(page.getByRole('link', { name: '使用 localhost 打开通行密钥入口' })).toHaveAttribute(
    'href',
    'http://localhost:31819/'
  )
})

test('sign in, issue, copy authorization text and revoke', async ({ page, context }) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write'])
  await login(page)
  await page.getByRole('button', { name: '确认授权并签发' }).click()
  await expect(page.getByLabel('本次访问令牌')).toHaveValue(/^[a-zA-Z0-9]{22}$/)
  const token = await page.getByLabel('本次访问令牌').inputValue()
  await page.getByRole('button', { name: '复制授权文本' }).click()
  const copied = await page.evaluate(() => navigator.clipboard.readText())
  expect(copied).toContain(token)
  expect(copied).toContain('I, an experienced developer')
  page.once('dialog', (dialog) => dialog.accept())
  await page.getByRole('row').filter({ hasText: '有效' }).last().getByRole('button', { name: '撤销' }).click()
  await expect(page.getByText('令牌已撤销。', { exact: true })).toBeVisible()
})

test('passkey registration, authentication and token issuance use real WebAuthn', async ({ page, context }) => {
  const cdp = await context.newCDPSession(page)
  await cdp.send('WebAuthn.enable')
  const { authenticatorId } = await cdp.send('WebAuthn.addVirtualAuthenticator', {
    options: {
      protocol: 'ctap2',
      transport: 'internal',
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  })
  await login(page)
  await page.getByRole('button', { name: '验证方式', exact: true }).click()
  await page.getByLabel('名称', { exact: true }).fill('browser virtual passkey')
  await page.getByRole('button', { name: '添加通行密钥' }).click()
  await expect(page.getByText('通行密钥已添加。', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: '退出管理' }).click()
  await expect(page.getByLabel('验证方式')).toHaveValue('passkey')
  await page.getByRole('button', { name: '使用通行密钥验证' }).click()
  await expect(page.getByRole('heading', { name: '签发访问令牌' })).toBeVisible()
  await page.getByRole('button', { name: '确认授权并签发' }).click()
  await expect(page.getByLabel('本次访问令牌')).toHaveValue(/^[a-zA-Z0-9]{22}$/)
  await page.getByRole('button', { name: '验证方式', exact: true }).click()
  page.once('dialog', (dialog) => dialog.accept())
  await page.getByRole('button', { name: '移除', exact: true }).click()
  await expect(page.getByText('通行密钥已移除。', { exact: true })).toBeVisible()
  await cdp.send('WebAuthn.removeVirtualAuthenticator', { authenticatorId })
})

test('mobile layout does not overflow and settings remain available', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await login(page)
  await page.getByRole('button', { name: '验证方式', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Authenticator', exact: true })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/mobile-credentials.png', fullPage: true })
  await page.getByRole('button', { name: '桌面授权', exact: true }).click()
  await page.getByRole('button', { name: '清除所有记忆' }).click()
  await expect(page.getByText('已清除所有代理的自动选择。')).toBeVisible()
})

test('consent content is literal text and returns choice plus note only through native IPC', async ({ page }) => {
  await page.setViewportSize({ width: 640, height: 780 })
  await page.addInitScript(() => {
    window.__LOCALSHELLD_PROMPT__ = {
      id: 'test',
      agent_name: 'Test agent',
      request_reason: 'literal <script>alert(1)</script>\nsecond line',
      scope: '仅回归测试，不执行命令',
      secret_keys: ['JENKINS_CI_TOKEN', 'OPENAI_API_KEY'],
      expires_at_ms: Date.now() + 60000,
    }
    window.ipc = {
      postMessage: (message: string) => {
        document.body.dataset.reply = message
      },
    }
  })
  await page.goto('/consent.html')
  await expect(page.getByLabel('代理提供的请求缘由')).toHaveText('literal <script>alert(1)</script>\nsecond line')
  await expect(page.getByLabel('请求注入的凭据')).toHaveValue('JENKINS_CI_TOKEN\nOPENAI_API_KEY')
  await expect(page.getByLabel('请求注入的凭据')).toHaveAttribute('readonly', '')
  await expect(page.getByRole('button', { name: '接受', exact: true })).toBeInViewport()
  await page.screenshot({ path: 'test-results/consent-secrets.png', fullPage: true })
  await page.getByLabel('附加说明', { exact: true }).fill('additional note')
  await page.getByRole('checkbox').check()
  await page.getByRole('button', { name: '接受', exact: true }).click()
  expect(await page.evaluate(() => JSON.parse(document.body.dataset.reply!))).toEqual({
    choice: 'accept',
    remember: true,
    btw: 'additional note',
  })
})

test('credentials require uppercase keys and support write-only creation, replacement and deletion', async ({
  page,
}) => {
  await login(page)
  await page.getByRole('button', { name: '凭据', exact: true }).click()
  const keyInput = page.getByLabel('Key', { exact: true })
  const valueInput = page.getByLabel('值', { exact: true })
  await expect(valueInput).toHaveAttribute('type', 'text')
  await keyInput.fill('lower_case')
  await valueInput.fill('fixture-browser-value')
  await page.getByRole('button', { name: '添加凭据', exact: true }).click()
  expect(await keyInput.evaluate((input: HTMLInputElement) => input.validity.patternMismatch)).toBe(true)
  await keyInput.fill('BROWSER_TEST_TOKEN')
  const savedResponse = page.waitForResponse((response) => response.url().endsWith('/v1/admin/secrets/put'))
  await page.getByRole('button', { name: '添加凭据', exact: true }).click()
  expect(await (await savedResponse).json()).toEqual({ saved: true })
  await expect(page.getByText('凭据已保存。', { exact: true })).toBeVisible()
  await expect(valueInput).toHaveValue('')
  const row = page.getByRole('listitem').filter({ hasText: 'BROWSER_TEST_TOKEN' })
  await expect(row).toBeVisible()
  await expect(page.getByText('fixture-browser-value', { exact: true })).toHaveCount(0)
  await page.screenshot({ path: 'test-results/desktop-secrets.png', fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/mobile-secrets.png', fullPage: true })
  await row.getByRole('button', { name: '更换', exact: true }).click()
  await expect(keyInput).toHaveAttribute('readonly', '')
  await expect(page.getByLabel('新值', { exact: true })).toHaveValue('')
  await page.getByLabel('新值', { exact: true }).fill('fixture-browser-rotated')
  await page.getByRole('button', { name: '保存新值', exact: true }).click()
  await expect(page.getByLabel('值', { exact: true })).toHaveValue('')
  await page.reload()
  await login(page)
  await page.getByRole('button', { name: '凭据', exact: true }).click()
  await expect(page.getByRole('listitem').filter({ hasText: 'BROWSER_TEST_TOKEN' })).toBeVisible()
  page.once('dialog', (dialog) => dialog.accept())
  await page
    .getByRole('listitem')
    .filter({ hasText: 'BROWSER_TEST_TOKEN' })
    .getByRole('button', { name: '删除', exact: true })
    .click()
  await expect(page.getByText('凭据已删除。', { exact: true })).toBeVisible()
  await expect(page.getByRole('listitem').filter({ hasText: 'BROWSER_TEST_TOKEN' })).toHaveCount(0)
})

test('desktop overview renders cleanly', async ({ page }) => {
  await login(page)
  await page.screenshot({ path: 'test-results/desktop-tokens.png', fullPage: true })
  await page.getByRole('button', { name: '桌面授权', exact: true }).click()
  await page.screenshot({ path: 'test-results/desktop-settings.png', fullPage: true })
})

test('credential file export is plaintext and import replaces the vault after confirmation', async ({ page }) => {
  await login(page)
  await page.getByRole('button', { name: '凭据', exact: true }).click()
  await expect(page.getByText('本机离线', { exact: true })).toBeVisible()
  const file = {
    format: 'localshelld-credentials',
    version: 1,
    credentials: { FILE_TEST: 'fixture-export-中文\nvalue' },
  }
  page.once('dialog', (dialog) => {
    expect(dialog.message()).toContain('替换全部')
    return dialog.accept()
  })
  await page.getByLabel('选择凭据文件').setInputFiles({
    name: 'credentials.json',
    mimeType: 'application/json',
    buffer: Buffer.from(JSON.stringify(file)),
  })
  await expect(page.getByText('凭据已导入。', { exact: true })).toBeVisible()
  await expect(page.getByRole('listitem').filter({ hasText: 'FILE_TEST' })).toBeVisible()
  page.once('dialog', (dialog) => {
    expect(dialog.message()).toContain('明文凭据')
    return dialog.accept()
  })
  const download = page.waitForEvent('download')
  await page.getByRole('button', { name: '导出文件', exact: true }).click()
  const downloaded = await download
  expect(JSON.parse(await readFile((await downloaded.path())!, 'utf8'))).toEqual(file)
  const invalid = { ...file, credentials: { lower: 'invalid' } }
  page.once('dialog', (dialog) => dialog.accept())
  await page
    .getByLabel('选择凭据文件')
    .setInputFiles({ name: 'invalid.json', mimeType: 'application/json', buffer: Buffer.from(JSON.stringify(invalid)) })
  await expect(page.getByRole('alert')).toContainText('key must match')
  await expect(page.getByRole('listitem').filter({ hasText: 'FILE_TEST' })).toBeVisible()
  page.once('dialog', (dialog) => dialog.accept())
  await page
    .getByRole('listitem')
    .filter({ hasText: 'FILE_TEST' })
    .getByRole('button', { name: '删除', exact: true })
    .click()
  await expect(page.getByText('凭据已删除。', { exact: true })).toBeVisible()
})
