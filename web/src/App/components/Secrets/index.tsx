import { type FormEvent, useEffect, useState } from 'react'

import { Button, Field, TextBox } from '@a1knla/cakeui'

import { Feedback } from '../../../Feedback'
import { api } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

export function Secrets({ session }: { session: string }) {
  const [keys, setKeys] = useState<string[] | null>(null)
  const [key, setKey] = useState('')
  const [value, setValue] = useState('')
  const [editing, setEditing] = useState(false)
  const action = useAction()

  async function refresh() {
    const result = await api<{ keys: string[] }>('/v1/admin/secrets', undefined, session)
    setKeys(result.keys)
  }

  useEffect(() => {
    void action.run(refresh)
  }, [session])

  function clearForm() {
    setKey('')
    setValue('')
    setEditing(false)
  }

  function save(event: FormEvent) {
    event.preventDefault()
    void action.run(async () => {
      if (!/^[A-Z_][A-Z0-9_]{0,127}$/.test(key)) throw new Error('Key 只能使用大写字母、数字和下划线，不能以数字开头。')
      if (!editing && keys?.includes(key)) throw new Error('此 Key 已存在，请在列表中选择更换。')
      await api('/v1/admin/secrets/put', { key, value }, session)
      clearForm()
      await refresh()
    }, '凭据已保存。')
  }

  function edit(name: string) {
    setKey(name)
    setValue('')
    setEditing(true)
    document.getElementById('secret-value')?.focus()
  }

  function remove(name: string) {
    if (!window.confirm(`删除凭据 ${name}？已有会话中的环境变量不会改变。`)) return
    void action.run(async () => {
      await api('/v1/admin/secrets/remove', { key: name }, session)
      if (key === name) clearForm()
      await refresh()
    }, '凭据已删除。')
  }

  return (
    <div className="secrets-view">
      <h2 className="section-title">凭据</h2>
      <p className="description">Key 即环境变量名，授权后注入 shell 会话。</p>
      <Feedback {...action} />
      <form className="form-stack" onSubmit={save}>
        <Field htmlFor="secret-key" label="Key" description="大写字母、数字和下划线，不能以数字开头。">
          <TextBox
            id="secret-key"
            value={key}
            onChange={(event) => setKey(event.currentTarget.value)}
            pattern="[A-Z_][A-Z0-9_]{0,127}"
            maxLength={128}
            placeholder="JENKINS_CI_TOKEN"
            autoComplete="off"
            spellCheck={false}
            readOnly={editing}
            required
          />
        </Field>
        <Field htmlFor="secret-value" label={editing ? '新值' : '值'}>
          <TextBox
            id="secret-value"
            type="password"
            value={value}
            onChange={(event) => setValue(event.currentTarget.value)}
            autoComplete="new-password"
            maxLength={16384}
            required
          />
        </Field>
        <div className="form-actions">
          <Button type="submit" variant="primary" disabled={action.busy || keys === null}>
            {editing ? '保存新值' : '添加凭据'}
          </Button>
          {editing && (
            <Button variant="ghost" disabled={action.busy} onClick={clearForm}>
              取消更换
            </Button>
          )}
        </div>
      </form>
      <ul className="secrets-list" aria-label="已保存的凭据">
        {keys?.map((name) => (
          <li className="secrets-row" key={name}>
            <span className="secrets-key">{name}</span>
            <div className="secrets-actions">
              <Button variant="ghost" disabled={action.busy} onClick={() => edit(name)}>
                更换
              </Button>
              <Button variant="ghost" disabled={action.busy} onClick={() => remove(name)}>
                删除
              </Button>
            </div>
          </li>
        ))}
      </ul>
      {keys?.length === 0 && <p className="empty-state">尚未添加凭据。</p>}
    </div>
  )
}
