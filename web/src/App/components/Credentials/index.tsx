import { type FormEvent, useEffect, useState } from 'react'

import { Button, Field, TextBox } from '@a1knla/cakeui'

import { Feedback } from '../../../Feedback'
import { type Settings, api, dateTime } from '../../../api'
import { registerPasskey } from '../../../passkeys'
import { useAction } from '../../../useAction'

import './index.scss'

type Enrollment = { enrollment_id: string; secret: string; uri: string }

export function Credentials({ session, onPasswordChanged }: { session: string; onPasswordChanged: () => void }) {
  const [settings, setSettings] = useState<Settings | null>(null)
  const [password, setPassword] = useState('')
  const [confirmation, setConfirmation] = useState('')
  const [enrollment, setEnrollment] = useState<Enrollment | null>(null)
  const [code, setCode] = useState('')
  const [name, setName] = useState('')
  const action = useAction()
  async function refresh() {
    setSettings(await api<Settings>('/v1/admin/settings', undefined, session))
  }
  useEffect(() => {
    void action.run(refresh)
  }, [session])
  function changePassword(event: FormEvent) {
    event.preventDefault()
    void action.run(async () => {
      if (password !== confirmation) throw new Error('两次输入的 Master Password 不一致。')
      await api('/v1/admin/password', { password }, session)
      onPasswordChanged()
    })
  }
  function finishTotp(event: FormEvent) {
    event.preventDefault()
    void action.run(async () => {
      if (!enrollment) return
      await api('/v1/admin/totp/finish', { enrollment_id: enrollment.enrollment_id, code }, session)
      setEnrollment(null)
      setCode('')
      await refresh()
    }, 'Authenticator 已启用。')
  }
  function addPasskey(event: FormEvent) {
    event.preventDefault()
    void action.run(async () => {
      await registerPasskey(name, session)
      setName('')
      await refresh()
    }, '通行密钥已添加。')
  }
  return (
    <div className="credentials-view">
      <h2 className="section-title">验证方式</h2>
      <p className="description">任一已启用的方式都可用于管理授权和签发访问令牌。</p>
      <Feedback {...action} />
      <section className="credential-section">
        <h3 className="subsection-title">Master Password</h3>
        <form className="form-stack" onSubmit={changePassword}>
          <Field htmlFor="new-password" label="新密码">
            <TextBox
              id="new-password"
              type="text"
              value={password}
              onChange={(event) => setPassword(event.currentTarget.value)}
              autoComplete="off"
            />
          </Field>
          <Field htmlFor="confirm-password" label="再次输入">
            <TextBox
              id="confirm-password"
              type="text"
              value={confirmation}
              onChange={(event) => setConfirmation(event.currentTarget.value)}
              autoComplete="off"
            />
          </Field>
          <div className="form-actions">
            <Button type="submit" disabled={action.busy}>
              更改密码并重新验证
            </Button>
          </div>
        </form>
      </section>
      <section className="credential-section">
        <div className="credential-heading">
          <h3 className="subsection-title">Authenticator</h3>
          <span className="credential-status">
            {settings ? (settings.totp_enabled ? '已启用' : '未启用') : '读取中'}
          </span>
        </div>
        <div className="form-actions">
          <Button
            disabled={action.busy}
            onClick={() =>
              void action.run(async () => {
                setEnrollment(await api<Enrollment>('/v1/admin/totp/start', {}, session))
                setCode('')
              })
            }
          >
            {settings?.totp_enabled ? '更换动态码密钥' : '设置动态码'}
          </Button>
          {settings?.totp_enabled && (
            <Button
              variant="ghost"
              disabled={action.busy}
              onClick={() =>
                void action.run(async () => {
                  if (!window.confirm('停用当前 Authenticator 动态码？')) return
                  await api('/v1/admin/totp/remove', {}, session)
                  await refresh()
                }, 'Authenticator 已停用。')
              }
            >
              停用
            </Button>
          )}
        </div>
        {enrollment && (
          <form className="totp-enrollment form-stack" onSubmit={finishTotp}>
            <p className="description">将此密钥添加到 Authenticator，再输入动态码确认。确认前原有设置保持不变。</p>
            <Field htmlFor="totp-secret" label="手动密钥">
              <TextBox id="totp-secret" className="credential-value" value={enrollment.secret} readOnly />
            </Field>
            <a className="content-link" href={enrollment.uri}>
              在此设备的 Authenticator 中打开
            </a>
            <Field htmlFor="totp-code" label="6 位动态码">
              <TextBox
                id="totp-code"
                type="text"
                inputMode="numeric"
                pattern="[0-9]{6}"
                required
                value={code}
                onChange={(event) => setCode(event.currentTarget.value)}
              />
            </Field>
            <div className="form-actions">
              <Button type="submit" variant="primary" disabled={action.busy}>
                确认启用
              </Button>
              <Button variant="ghost" onClick={() => setEnrollment(null)}>
                取消设置
              </Button>
            </div>
          </form>
        )}
      </section>
      <section className="credential-section">
        <h3 className="subsection-title">通行密钥</h3>
        {location.hostname !== 'localhost' && (
          <p className="description">
            通行密钥需要从{' '}
            <a className="content-link" href={`http://localhost:${location.port}/`}>
              localhost 入口
            </a>{' '}
            使用。
          </p>
        )}
        <form className="form-stack" onSubmit={addPasskey}>
          <Field htmlFor="passkey-name" label="名称">
            <TextBox
              id="passkey-name"
              placeholder="例如：这台电脑"
              required
              maxLength={80}
              value={name}
              onChange={(event) => setName(event.currentTarget.value)}
            />
          </Field>
          <div className="form-actions">
            <Button type="submit" disabled={action.busy}>
              添加通行密钥
            </Button>
          </div>
        </form>
        <ul className="passkey-list">
          {settings?.passkeys.map((key) => (
            <li key={key.id} className="passkey-row">
              <div className="passkey-details">
                <span>{key.name}</span>
                <span className="passkey-date">添加于 {dateTime(key.created_at)}</span>
              </div>
              <Button
                variant="ghost"
                disabled={action.busy}
                onClick={() =>
                  void action.run(async () => {
                    if (!window.confirm(`移除通行密钥“${key.name}”？`)) return
                    await api('/v1/admin/passkeys/remove', { id: key.id }, session)
                    await refresh()
                  }, '通行密钥已移除。')
                }
              >
                移除
              </Button>
            </li>
          ))}
        </ul>
        {settings?.passkeys.length === 0 && <p className="empty-state">尚未添加通行密钥。</p>}
      </section>
    </div>
  )
}
