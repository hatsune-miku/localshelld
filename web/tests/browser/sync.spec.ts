import { type ChildProcess, spawn } from 'node:child_process'
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { basename, dirname, join, resolve } from 'node:path'

import { type Page, expect, test } from '@playwright/test'

const origin = 'http://localhost:31821'
let server: ChildProcess
let directory = ''
let bootstrap = ''

test.beforeAll(async () => {
  directory = await mkdtemp(join(tmpdir(), 'localshelld-sync-browser-'))
  const executable = resolve(
    '..',
    'target',
    'debug',
    `localshelld-sync-server${process.platform === 'win32' ? '.exe' : ''}`
  )
  server = spawn(executable, ['--origin', origin, '--bind', '127.0.0.1:31821', '--data', directory], {
    windowsHide: true,
    stdio: 'ignore',
  })
  await expect
    .poll(
      async () => {
        try {
          return (await fetch(`${origin}/health`)).ok
        } catch {
          return false
        }
      },
      { timeout: 15000 }
    )
    .toBe(true)
  bootstrap = await readFile(join(directory, 'bootstrap-code'), 'utf8')
})

test.afterAll(async () => {
  bootstrap = ''
  if (server && server.exitCode === null) {
    const exited = new Promise<void>((resolve) => server.once('exit', () => resolve()))
    server.kill()
    await exited
  }
  // Only the exact, generated fixture directory is eligible for cleanup.
  if (
    directory &&
    dirname(resolve(directory)) === resolve(tmpdir()) &&
    basename(directory).startsWith('localshelld-sync-browser-')
  )
    await rm(directory, { recursive: true, force: true })
})

async function localLogin(page: Page, url: string) {
  await page.goto(url)
  await page.getByLabel('验证方式').selectOption('password')
  await page.getByLabel('Master Password', { exact: true }).fill('browser test')
  await page.getByRole('button', { name: '验证身份', exact: true }).click()
  await page.getByRole('button', { name: '凭据', exact: true }).click()
}

async function connect(local: Page, cloud: Page, name: string) {
  await local.getByText('连接同步服务', { exact: true }).click()
  await local.getByLabel('服务地址').fill(origin)
  await local.getByLabel('设备名称').fill(name)
  local.once('dialog', (dialog) => dialog.accept())
  await local.getByRole('button', { name: '连接服务', exact: true }).click()
  const link = local.getByRole('link', { name: '打开服务并授权此设备' })
  await expect(link).toBeVisible()
  const target = new URL((await link.getAttribute('href'))!)
  const code = new URLSearchParams(target.hash.slice(1)).get('device')!
  await cloud.getByLabel('设备验证码').fill(code)
  await cloud.getByRole('button', { name: '查看设备', exact: true }).click()
  await expect(cloud.locator('.sync-device-name')).toHaveText(name)
  await cloud.getByRole('button', { name: '授权此设备', exact: true }).click()
  await expect(local.getByText('云端同步', { exact: true })).toBeVisible({ timeout: 15000 })
}

test('passkey initialization, device pairing and two-client credential synchronization', async ({
  page: cloud,
  context,
}) => {
  test.setTimeout(90000)
  const cdp = await context.newCDPSession(cloud)
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
  await cloud.goto(origin)
  await expect(cloud.getByRole('heading', { name: '初始化同步服务' })).toBeVisible()
  await cloud.screenshot({ path: 'test-results/sync-initialize.png', fullPage: true })
  await cloud.getByLabel('初始化码').fill(bootstrap)
  await cloud.getByLabel('通行密钥名称').fill('Browser test passkey')
  await cloud.getByRole('button', { name: '创建通行密钥并初始化' }).click()
  await expect(cloud.getByText('通行密钥已添加。', { exact: true })).toBeVisible()
  await cloud.getByRole('button', { name: '退出登录', exact: true }).click()
  await cloud.getByRole('button', { name: '使用通行密钥登录', exact: true }).click()
  await expect(cloud.getByRole('heading', { name: '连接设备', exact: true })).toBeVisible()
  const a = await context.newPage()
  const b = await context.newPage()
  await localLogin(a, 'http://localhost:31819')
  await localLogin(b, 'http://localhost:31822')
  await connect(a, cloud, 'Test laptop A')
  await connect(b, cloud, 'Test laptop B')
  await a.getByLabel('Key', { exact: true }).fill('CLOUD_BROWSER_TOKEN')
  await a.getByLabel('值', { exact: true }).fill('fixture-cloud-value')
  await a.getByRole('button', { name: '添加凭据', exact: true }).click()
  await expect(a.getByText('凭据已保存。', { exact: true })).toBeVisible()
  await expect(b.getByRole('listitem').filter({ hasText: 'CLOUD_BROWSER_TOKEN' })).toBeVisible({ timeout: 15000 })
  await b
    .getByRole('listitem')
    .filter({ hasText: 'CLOUD_BROWSER_TOKEN' })
    .getByRole('button', { name: '更换', exact: true })
    .click()
  await b.getByLabel('新值', { exact: true }).fill('fixture-updated-on-b')
  await b.getByRole('button', { name: '保存新值', exact: true }).click()
  await expect(b.getByText('凭据已保存。', { exact: true })).toBeVisible()
  a.once('dialog', (dialog) => dialog.accept())
  const downloaded = a.waitForEvent('download')
  await a.getByRole('button', { name: '导出文件', exact: true }).click()
  expect(JSON.parse(await readFile((await (await downloaded).path())!, 'utf8')).credentials).toEqual({
    CLOUD_BROWSER_TOKEN: 'fixture-updated-on-b',
  })
  await a.screenshot({ path: 'test-results/sync-connected-desktop.png', fullPage: true })
  await a.setViewportSize({ width: 390, height: 844 })
  expect(await a.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await a.screenshot({ path: 'test-results/sync-connected-mobile.png', fullPage: true })
  await cloud.getByRole('button', { name: '刷新设备', exact: true }).click()
  await expect(cloud.getByText('Test laptop A', { exact: true })).toBeVisible()
  await cloud.screenshot({ path: 'test-results/sync-trusted-devices.png', fullPage: true })
  await cloud.setViewportSize({ width: 390, height: 844 })
  expect(await cloud.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await cloud.screenshot({ path: 'test-results/sync-trusted-mobile.png', fullPage: true })
  for (const local of [a, b]) {
    local.once('dialog', (dialog) => dialog.accept())
    await local.getByRole('button', { name: '使用离线存储', exact: true }).click()
    await expect(local.getByText('本机离线', { exact: true })).toBeVisible()
    await local.close()
  }
  await cdp.send('WebAuthn.removeVirtualAuthenticator', { authenticatorId })
})
