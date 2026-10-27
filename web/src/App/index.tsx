import { useEffect, useState } from 'react'

import { Button, CakeProvider } from '@a1knla/cakeui'

import { Credentials } from './components/Credentials'
import { DesktopSettings } from './components/DesktopSettings'
import { Login } from './components/Login'
import { Secrets } from './components/Secrets'
import { Tokens } from './components/Tokens'

import { type Health, type LoginResult, api } from '../api'

import './index.scss'

export function App() {
  const [health, setHealth] = useState<Health | null>(null)
  const [connectionError, setConnectionError] = useState('')
  const [login, setLogin] = useState<LoginResult | null>(null)
  const [section, setSection] = useState('tokens')
  async function connect() {
    try {
      setHealth(await api<Health>('/health'))
      setConnectionError('')
    } catch (error) {
      setConnectionError(error instanceof Error ? error.message : String(error))
    }
  }
  useEffect(() => {
    void connect()
  }, [])
  useEffect(() => {
    if (!login) return
    const timer = window.setTimeout(() => setLogin(null), Math.max(0, login.expires_at * 1000 - Date.now()))
    return () => window.clearTimeout(timer)
  }, [login])
  async function logout() {
    if (login) {
      try {
        await api('/v1/admin/logout', {}, login.session)
      } catch {
        /* Local logout must remain available offline. */
      } finally {
        setLogin(null)
      }
    }
  }
  const scope = health?.daemon === 'user' ? '当前用户' : '管理员 / root'
  function signedIn(result: LoginResult) {
    setSection('tokens')
    setLogin(result)
  }
  return (
    <CakeProvider mode="light" theme="blue" accentColor="#245aa8" className="app-theme">
      <div className="workspace">
        <header className="workspace-header">
          <h1 className="workspace-title">本机授权</h1>
          <span className="workspace-scope">{health ? scope : '正在连接本机服务'}</span>
          {login && (
            <Button variant="ghost" onClick={() => void logout()}>
              退出管理
            </Button>
          )}
        </header>
        {connectionError && (
          <div className="connection-error" role="alert">
            无法连接服务：{connectionError}
            <Button onClick={() => void connect()}>重新连接</Button>
          </div>
        )}
        {!login ? (
          <Login onLogin={signedIn} scope={scope} />
        ) : (
          <div className="workspace-body">
            <nav className="workspace-nav" aria-label="授权管理">
              {[
                ['tokens', '访问令牌'],
                ['secrets', '凭据'],
                ['credentials', '验证方式'],
                ['desktop', '桌面授权'],
              ].map(([id, label]) => (
                <Button
                  key={id}
                  variant="ghost"
                  className="workspace-nav-item"
                  aria-current={section === id ? 'page' : undefined}
                  onClick={() => setSection(id)}
                >
                  {label}
                </Button>
              ))}
              <p className="workspace-nav-note">管理登录在 10 分钟后失效。执行令牌遵循各自的有效期。</p>
            </nav>
            <main className="workspace-content">
              {section === 'tokens' && <Tokens session={login.session} scope={scope} />}
              {section === 'secrets' && <Secrets session={login.session} />}
              {section === 'credentials' && (
                <Credentials session={login.session} onPasswordChanged={() => setLogin(null)} />
              )}
              {section === 'desktop' && <DesktopSettings session={login.session} />}
            </main>
          </div>
        )}
        <footer className="workspace-footer">
          <span>localshelld {health?.version}</span>
          <span className="workspace-endpoint">{location.host}/mcp</span>
        </footer>
      </div>
    </CakeProvider>
  )
}
