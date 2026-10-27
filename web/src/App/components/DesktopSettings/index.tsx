import { type FormEvent, useEffect, useState } from 'react'

import { Button, Field, TextBox } from '@a1knla/cakeui'

import { TelegramSettings } from './components/TelegramSettings'

import { Feedback } from '../../../Feedback'
import { type Settings, api } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

export function DesktopSettings({ session }: { session: string }) {
  const [settings, setSettings] = useState<Settings | null>(null)
  const [code, setCode] = useState('')
  const [address, setAddress] = useState('127.0.0.1:32120')
  const action = useAction()
  async function refresh() {
    setSettings(await api<Settings>('/v1/admin/settings', undefined, session))
  }
  useEffect(() => {
    void action.run(refresh)
  }, [session])
  function pair(event: FormEvent) {
    event.preventDefault()
    void action.run(async () => {
      await api('/v1/admin/pairing/connect', { code: code.trim(), user_daemon: address.trim() }, session)
      setCode('')
      await refresh()
    }, '已完成可信配对。32119 的普通命令将由用户进程执行。')
  }
  return (
    <div className="desktop-settings">
      <h2 className="section-title">桌面授权</h2>
      <p className="description">代理可请求本机弹窗授权。接受、拒绝由用户选择，忽略会等待至 60 秒超时。</p>
      <Feedback {...action} />
      <section className="desktop-section">
        <h3 className="subsection-title">用户进程配对</h3>
        {settings?.paired_peer && <p className="description">已配对：{settings.paired_peer}</p>}
        {settings?.daemon === 'user' ? (
          <div className="form-stack">
            {!settings.desktop_available && (
              <p className="desktop-warning">
                此用户进程没有图形桌面。请在登录桌面中启动用户 daemon；无头模式无法显示授权弹窗。
              </p>
            )}
            <p className="description">
              生成配对码后，在管理员入口完成配对。该用户进程将能代表设备所有者确认管理员授权。
            </p>
            <div className="form-actions">
              <Button
                disabled={action.busy}
                onClick={() =>
                  void action.run(async () => {
                    const result = await api<{ code: string }>('/v1/admin/pairing/new', {}, session)
                    setCode(result.code)
                    await refresh()
                  })
                }
              >
                生成配对码
              </Button>
            </div>
            {code && (
              <Field htmlFor="generated-pairing" label="配对码">
                <TextBox id="generated-pairing" value={code} readOnly className="credential-value" />
              </Field>
            )}
            {code && (
              <Button onClick={() => void action.run(() => navigator.clipboard.writeText(code), '配对码已复制。')}>
                复制配对码
              </Button>
            )}
            <a className="content-link" href="http://localhost:32119/" target="_blank" rel="noreferrer">
              打开管理员入口
            </a>
          </div>
        ) : (
          <form className="form-stack" onSubmit={pair}>
            <p className="description">
              先在{' '}
              <a className="content-link" href="http://localhost:32120/" target="_blank" rel="noreferrer">
                用户进程管理页
              </a>{' '}
              验证身份并生成配对码，再填写到这里。仅与自己信任的桌面进程配对。
            </p>
            <Field htmlFor="user-daemon-address" label="用户进程地址">
              <TextBox
                id="user-daemon-address"
                required
                value={address}
                onChange={(event) => setAddress(event.currentTarget.value)}
              />
            </Field>
            <Field htmlFor="pairing-code" label="配对码">
              <TextBox
                id="pairing-code"
                required
                value={code}
                onChange={(event) => setCode(event.currentTarget.value)}
              />
            </Field>
            <div className="form-actions">
              <Button type="submit" disabled={action.busy}>
                {settings?.paired_peer ? '重新配对' : '配对用户进程'}
              </Button>
            </div>
          </form>
        )}
        {settings?.paired_peer && (
          <Button
            className="disconnect-button"
            variant="ghost"
            disabled={action.busy}
            onClick={() =>
              void action.run(async () => {
                await api('/v1/admin/pairing/remove', {}, session)
                setCode('')
                await refresh()
              }, '已取消此进程的配对。')
            }
          >
            取消配对
          </Button>
        )}
      </section>
      <TelegramSettings session={session} />
      <section className="desktop-section">
        <h3 className="subsection-title">记住的选择</h3>
        <p className="description">按代理提交的名称匹配。名称是自报信息，不是身份认证；重置不会撤销已经建立的会话。</p>
        {settings && Object.keys(settings.remembered).length ? (
          <ul className="remembered-list">
            {Object.entries(settings.remembered).map(([name, allowed]) => (
              <li className="remembered-row" key={name}>
                <span className="remembered-name">{name}</span>
                <span>{allowed ? '接受' : '拒绝'}</span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="empty-state">没有记住的选择。</p>
        )}
        <Button
          disabled={action.busy}
          onClick={() =>
            void action.run(async () => {
              await api('/v1/admin/remembered/reset', {}, session)
              await refresh()
            }, '已清除所有代理的自动选择。')
          }
        >
          清除所有记忆
        </Button>
      </section>
    </div>
  )
}
