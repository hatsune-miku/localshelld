import { type FormEvent, useEffect, useState } from 'react'

import { Button, CakeProvider, Field, TextBox } from '@a1knla/cakeui'

import { Feedback } from '../Feedback'
import { api, dateTime } from '../api'
import { useAction } from '../useAction'
import { type Assertion, type Creation, createCredential, getCredential } from '../webauthn'

import './index.scss'

type Info = { protocol: number; initialized: boolean; origin: string }
type Tokens = { access_token: string; expires_at: number }
type Trust = {
  passkeys: { id: string; name: string }[]
  sessions: { id: string; name: string; created_at: number; expires_at: number }[]
}
type Device = { name: string; user_code: string; approved: boolean }

export function Sync() {
  const [info, setInfo] = useState<Info | null>(null)
  const [login, setLogin] = useState<Tokens | null>(null)
  const [bootstrap, setBootstrap] = useState('')
  const [name, setName] = useState('我的通行密钥')
  const [userCode, setUserCode] = useState(() => new URLSearchParams(location.hash.slice(1)).get('device') ?? '')
  const [device, setDevice] = useState<Device | null>(null)
  const [trust, setTrust] = useState<Trust | null>(null)
  const action = useAction()

  useEffect(() => {
    void action.run(async () => setInfo(await api<Info>('/sync/v1/info')))
  }, [])
  useEffect(() => {
    if (!login) return
    const timer = window.setTimeout(
      () => {
        setLogin(null)
        setTrust(null)
        setDevice(null)
      },
      Math.max(0, login.expires_at * 1000 - Date.now())
    )
    return () => window.clearTimeout(timer)
  }, [login])

  async function signedIn(result: Tokens) {
    setLogin(result)
    setBootstrap('')
    setInfo(await api<Info>('/sync/v1/info'))
    setTrust(await api<Trust>('/sync/v1/sessions', undefined, result.access_token))
    if (userCode) setDevice(await api<Device>('/sync/v1/device/inspect', { user_code: userCode }, result.access_token))
  }

  function register(event: FormEvent) {
    event.preventDefault()
    void action.run(async () => {
      const challenge = await api<{ challenge_id: string; options: { publicKey: Creation } }>(
        '/sync/v1/passkeys/register/start',
        { name, bootstrap_code: bootstrap },
        login?.access_token
      )
      const result = await api<Tokens>(
        '/sync/v1/passkeys/register/finish',
        {
          challenge_id: challenge.challenge_id,
          credential: await createCredential(challenge.options.publicKey),
        },
        login?.access_token
      )
      await signedIn(result)
    }, '通行密钥已添加。')
  }

  function signIn() {
    void action.run(async () => {
      const challenge = await api<{ challenge_id: string; options: { publicKey: Assertion } }>(
        '/sync/v1/passkeys/authenticate/start',
        {}
      )
      await signedIn(
        await api<Tokens>('/sync/v1/passkeys/authenticate/finish', {
          challenge_id: challenge.challenge_id,
          credential: await getCredential(challenge.options.publicKey),
        })
      )
    })
  }

  function inspect(event: FormEvent) {
    event.preventDefault()
    void action.run(async () =>
      setDevice(await api<Device>('/sync/v1/device/inspect', { user_code: userCode }, login?.access_token))
    )
  }

  function approve() {
    if (!device) return
    void action.run(async () => {
      await api('/sync/v1/device/approve', { user_code: device.user_code }, login?.access_token)
      setDevice({ ...device, approved: true })
      history.replaceState(null, '', location.pathname)
    }, '设备已授权，可以返回本机页面。')
  }

  function remove(kind: 'passkeys' | 'session', id: string, label: string) {
    if (!window.confirm(`移除${label}？`)) return
    void action.run(async () => {
      await api(`/sync/v1/${kind}/${kind === 'passkeys' ? 'remove' : 'revoke'}`, { id }, login?.access_token)
      setTrust(await api<Trust>('/sync/v1/sessions', undefined, login?.access_token))
    }, '已移除。')
  }

  function logout() {
    void action.run(async () => {
      try {
        await api('/sync/v1/session/revoke', { id: '' }, login?.access_token)
      } finally {
        setLogin(null)
        setTrust(null)
        setDevice(null)
      }
    })
  }

  return (
    <CakeProvider mode="light" theme="blue" accentColor="#245aa8" className="app-theme">
      <main className="sync-page">
        <header className="sync-header">
          <h1 className="sync-title">凭据同步</h1>
          <span className="sync-brand">trustd</span>
          {login && (
            <Button variant="ghost" onClick={logout} disabled={action.busy}>
              退出登录
            </Button>
          )}
        </header>
        <div className="sync-content">
          <Feedback {...action} />
          {!info && <p className="description">正在连接同步服务。</p>}
          {info?.initialized && !login && (
            <section className="sync-section">
              <h2 className="section-title">使用通行密钥登录</h2>
              <Button variant="primary" onClick={signIn} loading={action.busy}>
                使用通行密钥登录
              </Button>
            </section>
          )}
          {info && (!info.initialized || login) && (
            <>
              {!info.initialized && <h2 className="section-title">初始化同步服务</h2>}
              {login && (
                <section className="sync-section">
                  <h2 className="section-title">连接设备</h2>
                  <form className="form-stack" onSubmit={inspect}>
                    <Field htmlFor="device-code" label="设备验证码">
                      <TextBox
                        id="device-code"
                        value={userCode}
                        onChange={(event) => {
                          setUserCode(event.currentTarget.value.trim().toUpperCase())
                          setDevice(null)
                        }}
                        autoComplete="off"
                        spellCheck={false}
                        required
                      />
                    </Field>
                    <div className="form-actions">
                      <Button type="submit" disabled={action.busy}>
                        查看设备
                      </Button>
                    </div>
                  </form>
                  {device && (
                    <div className="sync-device">
                      <strong className="sync-device-name">{device.name}</strong>
                      <span className="sync-device-code">{device.user_code}</span>
                      <p className="sync-device-hint">核对本机显示的验证码。授权后，此设备可读取和修改全部云端凭据。</p>
                      <Button variant="primary" onClick={approve} disabled={action.busy || device.approved}>
                        {device.approved ? '已授权' : '授权此设备'}
                      </Button>
                    </div>
                  )}
                </section>
              )}
              <section className="sync-section">
                {login && <h2 className="section-title">通行密钥</h2>}
                <form className="form-stack" onSubmit={register}>
                  {!info.initialized && (
                    <Field
                      htmlFor="bootstrap-code"
                      label="初始化码"
                      description="位于服务器数据目录的 bootstrap-code 文件中。"
                    >
                      <TextBox
                        id="bootstrap-code"
                        value={bootstrap}
                        onChange={(event) => setBootstrap(event.currentTarget.value.trim())}
                        autoComplete="off"
                        spellCheck={false}
                        required
                      />
                    </Field>
                  )}
                  <Field htmlFor="passkey-name" label="通行密钥名称">
                    <TextBox
                      id="passkey-name"
                      value={name}
                      onChange={(event) => setName(event.currentTarget.value)}
                      maxLength={80}
                      required
                    />
                  </Field>
                  <div className="form-actions">
                    <Button type="submit" variant="primary" disabled={action.busy}>
                      {info.initialized ? '添加通行密钥' : '创建通行密钥并初始化'}
                    </Button>
                  </div>
                </form>
                <ul className="sync-list">
                  {trust?.passkeys.map((key) => (
                    <li className="sync-row" key={key.id}>
                      <span className="sync-row-name">{key.name}</span>
                      <Button
                        variant="ghost"
                        disabled={action.busy || trust.passkeys.length === 1}
                        onClick={() => remove('passkeys', key.id, '通行密钥')}
                      >
                        移除
                      </Button>
                    </li>
                  ))}
                </ul>
              </section>
              {trust && (
                <section className="sync-section">
                  <h2 className="section-title">已连接设备</h2>
                  <ul className="sync-list">
                    {trust.sessions.map((session) => (
                      <li className="sync-row" key={session.id}>
                        <div className="sync-row-name">
                          <span className="sync-session-name">{session.name}</span>
                          <span className="sync-session-expiry">有效期至 {dateTime(session.expires_at)}</span>
                        </div>
                        <Button
                          variant="ghost"
                          disabled={action.busy}
                          onClick={() => remove('session', session.id, '设备授权')}
                        >
                          撤销授权
                        </Button>
                      </li>
                    ))}
                  </ul>
                  {trust.sessions.length === 0 && (
                    <p className="empty-state">尚未连接设备。在本机凭据页面设置此服务地址。</p>
                  )}
                  <Button
                    variant="ghost"
                    disabled={action.busy}
                    onClick={() =>
                      void action.run(async () =>
                        setTrust(await api<Trust>('/sync/v1/sessions', undefined, login?.access_token))
                      )
                    }
                  >
                    刷新设备
                  </Button>
                </section>
              )}
            </>
          )}
        </div>
        <footer className="sync-footer">{info?.origin ?? location.origin}</footer>
      </main>
    </CakeProvider>
  )
}
