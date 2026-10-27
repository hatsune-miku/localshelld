import { useEffect, useState } from 'react'

import { Button, CakeProvider, CheckBox, Field, TextArea } from '@a1knla/cakeui'

import './index.scss'

type Prompt = {
  id: string
  agent_name: string
  request_reason: string
  scope: string
  secret_keys?: string[]
  expires_at_ms: number
}
declare global {
  interface Window {
    __LOCALSHELLD_PROMPT__?: Prompt
    ipc?: { postMessage: (message: string) => void }
  }
}

export function Consent() {
  const prompt = window.__LOCALSHELLD_PROMPT__
  const [remember, setRemember] = useState(false)
  const [btw, setBtw] = useState('')
  const [now, setNow] = useState(Date.now())
  const [sent, setSent] = useState(false)
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 250)
    return () => window.clearInterval(timer)
  }, [])
  function choose(choice: 'accept' | 'deny' | 'ignore') {
    if (!prompt || !window.ipc || sent || Date.now() >= prompt.expires_at_ms) return
    setSent(true)
    window.ipc.postMessage(JSON.stringify({ choice, remember: choice !== 'ignore' && remember, btw }))
  }
  const remaining = prompt ? Math.max(0, Math.ceil((prompt.expires_at_ms - now) / 1000)) : 0
  return (
    <CakeProvider theme="blue" mode="light" accentColor="#245aa8" className="app-theme">
      <main className="consent-view">
        {!prompt || !window.ipc ? (
          <p role="alert">授权只能在本机原生弹窗中完成。此网页不提供批准接口。</p>
        ) : (
          <>
            <header className="consent-heading">
              <h1 className="section-title">请求执行本机命令</h1>
              <span className="consent-time">{remaining} 秒</span>
            </header>
            <p className="consent-agent">{prompt.agent_name}</p>
            <p className="description">授权范围：{prompt.scope}</p>
            {!!prompt.secret_keys?.length && (
              <Field htmlFor="consent-secrets" label="请求注入的凭据">
                <TextArea
                  id="consent-secrets"
                  className="consent-secrets"
                  value={prompt.secret_keys.join('\n')}
                  readOnly
                  rows={3}
                />
              </Field>
            )}
            <section className="consent-reason" aria-label="代理提供的请求缘由">
              {prompt.request_reason}
            </section>
            <Field htmlFor="consent-note" label="附加说明" description="随本次工具应答返回给代理。">
              <TextArea
                id="consent-note"
                aria-describedby="consent-note-help"
                value={btw}
                onChange={(event) => setBtw(event.currentTarget.value)}
                maxLength={4096}
                rows={3}
              />
            </Field>
            <CheckBox
              className="consent-remember"
              checked={remember}
              onChange={(event) => setRemember(event.currentTarget.checked)}
            >
              记住对 {prompt.agent_name} 的选择；新增凭据需再次授权
            </CheckBox>
            <div className="consent-actions">
              <Button variant="ghost" disabled={sent || !remaining} onClick={() => choose('ignore')}>
                忽略
              </Button>
              <Button disabled={sent || !remaining} onClick={() => choose('deny')}>
                拒绝
              </Button>
              <Button variant="primary" disabled={sent || !remaining} onClick={() => choose('accept')}>
                接受
              </Button>
            </div>
            {!remaining && (
              <p className="consent-expired" role="status">
                请求已超时，未授予权限。
              </p>
            )}
          </>
        )}
      </main>
    </CakeProvider>
  )
}
