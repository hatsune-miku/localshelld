import { type Page, expect, test } from '@playwright/test'

async function login(page: Page) {
  await page.goto('/')
  await page.getByLabel('验证方式').selectOption('password')
  await page.getByLabel('Master Password', { exact: true }).fill('browser test')
  await page.getByRole('button', { name: '验证身份', exact: true }).click()
  await page.getByRole('button', { name: '桌面授权', exact: true }).click()
}

test('configure, bind, test and remove Telegram with a cloud status and no token echo', async ({ page }) => {
  const commands: Record<string, unknown>[] = []
  let configured = false
  let bound = false
  await page.route('**/v1/admin/telegram', async (route) => {
    const command = route.request().postDataJSON()
    commands.push(command)
    let result: object = {}
    switch (command.action) {
      case 'status':
        result = {
          configured,
          enabled: true,
          bot_username: 'fixture_bot',
          binding: bound ? { user_id: 42, chat_id: 42 } : null,
          mode: 'cloud',
          revision: 7,
        }
        break
      case 'configure':
        configured = true
        result = { saved: true }
        break
      case 'bind_start':
        result = { code: 'abcdef', url: 'https://t.me/fixture_bot?start=abcdef', expires_in: 900 }
        break
      case 'bind_poll':
        bound = true
        result = { bound: true }
        break
      case 'test':
        result = { sent: true }
        break
      case 'remove':
        configured = false
        bound = false
        result = { removed: true }
        break
    }
    await route.fulfill({ json: result })
  })
  await login(page)
  await expect(page.getByText('尚未配置 bot · 云端中转 · 版本 7')).toBeVisible()
  await page.getByLabel('Bot Token', { exact: true }).fill('123456:fixture-token-not-a-real-secret')
  await page.getByRole('button', { name: '保存 Telegram 设置', exact: true }).click()
  await expect(page.getByLabel('Bot Token', { exact: true })).toHaveValue('')
  await page.getByRole('button', { name: '绑定 Telegram 账号' }).click()
  await expect(page.getByRole('link', { name: '在 Telegram 中绑定' })).toHaveAttribute(
    'href',
    'https://t.me/fixture_bot?start=abcdef'
  )
  await expect(page.getByText('@fixture_bot · 已绑定 · 云端中转 · 版本 7')).toBeVisible()
  await page.getByRole('button', { name: '发送测试消息' }).click()
  await expect(page.getByText('测试消息已发送。')).toBeVisible()
  await page.getByRole('button', { name: '保存 Telegram 设置', exact: true }).click()
  await expect(page.getByText('Telegram 设置已保存。')).toBeVisible()
  expect(commands.filter((c) => c.action === 'configure').at(-1)?.token).toBeNull()
  await page.screenshot({ path: 'test-results/telegram-settings-desktop.png', fullPage: true })
  await page.setViewportSize({ width: 390, height: 844 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  await page.screenshot({ path: 'test-results/telegram-settings-mobile.png', fullPage: true })
  await page.getByRole('button', { name: '移除 Telegram 配置' }).click()
  await expect(page.getByText('尚未配置 bot · 云端中转 · 版本 7')).toBeVisible()
})

test('shows configuration errors without clearing an unsaved token', async ({ page }) => {
  await page.route('**/v1/admin/telegram', async (route) => {
    const command = route.request().postDataJSON()
    await route.fulfill(
      command.action === 'status'
        ? { json: { configured: false, enabled: false, mode: 'cloud', revision: 0 } }
        : { status: 400, json: { error: 'Telegram Bot Token 无效。' } }
    )
  })
  await login(page)
  await page.getByLabel('Bot Token', { exact: true }).fill('invalid-fixture')
  await page.getByRole('button', { name: '保存 Telegram 设置', exact: true }).click()
  await expect(page.getByText('Telegram Bot Token 无效。')).toBeVisible()
  await expect(page.getByLabel('Bot Token', { exact: true })).toHaveValue('invalid-fixture')
})

test('offline mode requires cloud connection and exposes no bot controls', async ({ page }) => {
  const commands: string[] = []
  await page.route('**/v1/admin/telegram', async (route) => {
    commands.push(route.request().postDataJSON().action)
    await route.fulfill({ json: { configured: false, enabled: false, mode: 'offline' } })
  })
  await login(page)
  await expect(page.getByText('请先在云同步中连接云服务。')).toBeVisible()
  await expect(page.getByLabel('Bot Token', { exact: true })).toHaveCount(0)
  await expect(page.getByRole('button', { name: '保存 Telegram 设置', exact: true })).toHaveCount(0)
  await expect(page.getByLabel('启用 Telegram 授权')).toHaveCount(0)
  expect(commands).toEqual(['status'])
})
