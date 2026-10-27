import { type FormEvent, useEffect, useState } from 'react'

import { Button, CheckBox, Field, TextBox } from '@a1knla/cakeui'

import { Feedback } from '../../../../../Feedback'
import { api } from '../../../../../api'
import { useAction } from '../../../../../useAction'

import './index.scss'

type Status = {
  configured: boolean
  enabled: boolean
  bot_username: string | null
  binding: { user_id: number; chat_id: number } | null
  mode: 'cloud' | 'offline'
  revision?: number
  error: string | null
}
type Binding = { code: string; url: string; expires_in: number }

export function TelegramSettings({ session }: { session: string }) {
  const [status, setStatus] = useState<Status | null>(null)
  const [token, setToken] = useState('')
  const [enabled, setEnabled] = useState(true)
  const [binding, setBinding] = useState<Binding | null>(null)
  const action = useAction()
  async function send<T>(command: object) {
    return api<T>('/v1/admin/telegram', command, session)
  }
  async function refresh() {
    const next = await send<Status>({ action: 'status' })
    setStatus(next)
    setEnabled(next.enabled || !next.configured)
  }
  useEffect(() => {
    void action.run(refresh)
  }, [session])
  useEffect(() => {
    if (!binding) return
    let stopped = false
    let timer: ReturnType<typeof setTimeout>
    const expires = Date.now() + binding.expires_in * 1000
    async function poll() {
      if (stopped) return
      if (Date.now() >= expires) {
        setBinding(null)
        return
      }
      try {
        const result = await send<{ bound?: boolean }>({ action: 'bind_poll', code: binding!.code })
        if (stopped) return
        if (result.bound) {
          setBinding(null)
          await action.run(refresh, 'Telegram 账号已绑定。')
          return
        }
      } catch (error) {
        if (stopped) return
        setBinding(null)
        await action.run(async () => {
          throw error
        })
        return
      }
      timer = setTimeout(poll, 3000)
    }
    timer = setTimeout(poll, 1500)
    return () => {
      stopped = true
      clearTimeout(timer)
    }
  }, [binding, session])
  function save(event: FormEvent) {
    event.preventDefault()
    void action.run(async () => {
      await send({ action: 'configure', token: token.trim() || null, enabled })
      setToken('')
      setBinding(null)
      await refresh()
    }, 'Telegram 设置已保存。')
  }
  if (status?.mode === 'offline') {
    return (
      <section className="desktop-section telegram-settings">
        <h3 className="subsection-title">Telegram 授权</h3>
        <p className="description" role="status">
          请先在云同步中连接云服务。
        </p>
      </section>
    )
  }
  return (
    <section className="desktop-section telegram-settings">
      <h3 className="subsection-title">Telegram 授权</h3>
      <p className="description">与桌面弹窗同时可用，在任一端处理即可。</p>
      <Feedback {...action} />
      {status && (
        <p className="telegram-status" role="status">
          {status.configured ? `@${status.bot_username} · ${status.binding ? '已绑定' : '未绑定'}` : '尚未配置 bot'}
          {' · '}
          {`云端中转 · 版本 ${status.revision}`}
        </p>
      )}
      {status?.error && (
        <p className="desktop-warning" role="alert">
          {status.error}
        </p>
      )}
      <form className="form-stack" onSubmit={save}>
        <Field htmlFor="telegram-token" label="Bot Token">
          <TextBox
            id="telegram-token"
            autoComplete="off"
            spellCheck={false}
            value={token}
            required={!status?.configured}
            placeholder={status?.configured ? '已保存，留空保留原值' : '填写 BotFather 提供的 token'}
            onChange={(event) => setToken(event.currentTarget.value)}
          />
        </Field>
        <CheckBox checked={enabled} onChange={(event) => setEnabled(event.currentTarget.checked)}>
          启用 Telegram 授权
        </CheckBox>
        <div className="form-actions">
          <Button type="submit" disabled={action.busy || !status}>
            保存 Telegram 设置
          </Button>
          {status?.configured && (
            <Button
              type="button"
              disabled={action.busy}
              onClick={() => void action.run(async () => setBinding(await send<Binding>({ action: 'bind_start' })))}
            >
              {status.binding ? '重新绑定账号' : '绑定 Telegram 账号'}
            </Button>
          )}
          {status?.binding && (
            <Button
              type="button"
              disabled={action.busy}
              onClick={() => void action.run(() => send({ action: 'test' }), '测试消息已发送。')}
            >
              发送测试消息
            </Button>
          )}
        </div>
      </form>
      {binding && (
        <div className="telegram-binding">
          <a className="content-link" href={binding.url} target="_blank" rel="noreferrer">
            在 Telegram 中绑定
          </a>
          <span className="description">打开链接并选择 Start，正在等待确认。</span>
        </div>
      )}
      {status?.configured && (
        <Button
          className="disconnect-button"
          variant="ghost"
          disabled={action.busy}
          onClick={() =>
            void action.run(async () => {
              await send({ action: 'remove' })
              setBinding(null)
              setToken('')
              await refresh()
            }, 'Telegram 配置已移除。')
          }
        >
          移除 Telegram 配置
        </Button>
      )}
    </section>
  )
}
