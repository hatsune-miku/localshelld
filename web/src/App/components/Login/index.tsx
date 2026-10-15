import { type FormEvent, useState } from 'react'

import { Button, ComboBox, Field, TextBox } from '@a1knla/cakeui'

import { Feedback } from '../../../Feedback'
import { type LoginResult, api } from '../../../api'
import { signInWithPasskey } from '../../../passkeys'
import { useAction } from '../../../useAction'

import './index.scss'

export function Login({ onLogin, scope }: { onLogin: (login: LoginResult) => void; scope: string }) {
  const [kind, setKind] = useState('totp')
  const [value, setValue] = useState('')
  const action = useAction()
  function submit(event: FormEvent) {
    event.preventDefault()
    void action.run(async () =>
      onLogin(
        kind === 'passkey'
          ? await signInWithPasskey()
          : await api<LoginResult>('/v1/admin/login', { credential: { type: kind, value } })
      )
    )
  }
  return (
    <main className="login-view">
      <h2 className="section-title">验证后管理授权</h2>
      <p className="description">此入口管理{scope}权限。尊重用户的个人隐私。</p>
      <form className="form-stack" onSubmit={submit}>
        <Field htmlFor="login-kind" label="验证方式">
          <ComboBox id="login-kind" value={kind} onChange={(event) => setKind(event.currentTarget.value)}>
            <option value="password">Master Password</option>
            <option value="totp">Authenticator 动态码</option>
            <option value="passkey">通行密钥</option>
          </ComboBox>
        </Field>
        {kind !== 'passkey' && (
          <Field htmlFor="login-value" label={kind === 'totp' ? '6 位动态码' : 'Master Password'}>
            <TextBox
              id="login-value"
              type="text"
              value={value}
              onChange={(event) => setValue(event.currentTarget.value)}
              autoComplete="off"
              inputMode={kind === 'totp' ? 'numeric' : 'text'}
              required={kind === 'totp'}
            />
          </Field>
        )}
        {kind === 'passkey' && location.hostname !== 'localhost' && (
          <a className="content-link" href={`http://localhost:${location.port}/`}>
            使用 localhost 打开通行密钥入口
          </a>
        )}
        <div className="form-actions">
          <Button type="submit" variant="primary" loading={action.busy}>
            {kind === 'passkey' ? '使用通行密钥验证' : '验证身份'}
          </Button>
        </div>
      </form>
      <Feedback {...action} />
    </main>
  )
}
