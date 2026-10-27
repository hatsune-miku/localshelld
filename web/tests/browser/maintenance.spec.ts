import { type Page, expect, test } from '@playwright/test'

async function setup(page: Page, registered = true) {
  const info = {
    version: '0.1.0',
    platform: 'windows',
    program: 'C:\\Program Files\\localshelld',
    system_config: 'C:\\ProgramData\\localshelld\\system\\config.toml',
    user_config: 'C:\\Users\\Example\\AppData\\Roaming\\localshelld\\user.toml',
    registered,
    registered_executable: registered ? 'D:\\bin\\localshelld.exe' : null,
    registered_config: registered ? 'D:\\old\\config.toml' : null,
    system_running: registered,
    user_initialized: registered,
    shell: 'C:\\Program Files\\PowerShell\\7\\pwsh.exe',
    shell_available: true,
    elevation_available: true,
    user_url: 'http://localhost:32120/',
    system_url: 'http://localhost:32119/',
    job: { running: false, stage: '', error: null, completed: null as string | null },
  }
  const installs: unknown[] = []
  await page.route('**/setup/v1/**', async (route) => {
    expect(route.request().headers().authorization).toBe('Bearer browser-fixture')
    const path = new URL(route.request().url()).pathname
    if (path.endsWith('/install')) {
      installs.push(route.request().postDataJSON())
      info.job = { running: false, stage: 'done', error: null, completed: 'install' }
    }
    await route.fulfill({ json: path.endsWith('/status') ? info : { started: true } })
  })
  await page.goto('/maintenance.html#browser-fixture')
  await expect(page.getByRole('button', { name: '下一步' })).toBeVisible()
  return installs
}

test('maintenance checks locations, preserves credentials and finishes without real elevation', async ({ page }) => {
  const installs = await setup(page)
  await expect(page.getByRole('heading', { name: '已检测到安装' })).toBeVisible()
  await expect(page.getByText('C:\\Program Files\\localshelld', { exact: true })).toBeVisible()
  await page.screenshot({ path: 'test-results/maintenance-desktop.png', fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/maintenance-mobile.png', fullPage: true })
  await page.getByRole('button', { name: '下一步' }).click()
  await expect(page.getByLabel('主密码', { exact: true })).toHaveCount(0)
  await page.getByRole('button', { name: '安装并修复' }).click()
  await expect(page.getByRole('heading', { name: '安装完成' })).toBeVisible()
  expect(installs).toEqual([{ password: null }])
  await expect(page.getByRole('link', { name: '打开系统管理' })).toHaveAttribute('href', 'http://localhost:32119/')
  await page.getByRole('button', { name: '完成', exact: true }).click()
  await expect(page.getByRole('heading', { name: '向导已关闭' })).toBeVisible()
})

test('initialization keeps plaintext input, validates confirmation and accepts explicitly empty passwords', async ({
  page,
}) => {
  const installs = await setup(page, false)
  await page.getByRole('button', { name: '下一步' }).click()
  await expect(page.getByLabel('主密码', { exact: true })).toHaveAttribute('type', 'text')
  await page.getByLabel('主密码', { exact: true }).fill('different')
  await page.getByRole('button', { name: '开始安装' }).click()
  await expect(page.getByText('两次输入的密码不一致。')).toBeVisible()
  expect(installs).toEqual([])
  await page.getByLabel('主密码', { exact: true }).fill('')
  await page.getByRole('button', { name: '开始安装' }).click()
  await expect(page.getByRole('heading', { name: '安装完成' })).toBeVisible()
  expect(installs).toEqual([{ password: '' }])
})

test('a direct page without the launch capability cannot offer installation', async ({ page }) => {
  await page.goto('/maintenance.html')
  await expect(page.getByRole('heading', { name: '重新打开向导' })).toBeVisible()
  await expect(page.getByRole('button', { name: '下一步' })).toHaveCount(0)
})
