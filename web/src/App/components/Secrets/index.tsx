import { type ChangeEvent, type FormEvent, useEffect, useRef, useState } from 'react'

import { Button, Field, TextBox } from '@a1knla/cakeui'

import { SyncSettings } from './components/SyncSettings'

import { Feedback } from '../../../Feedback'
import { api } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

export function Secrets({ session }: { session: string }) {
  const [keys, setKeys] = useState<string[] | null>(null)
  const [key, setKey] = useState('')
  const [value, setValue] = useState('')
  const [editing, setEditing] = useState(false)
  const fileInput = useRef<HTMLInputElement>(null)
  const action = useAction()

  async function refresh() {
    const result = await api<{ keys: string[] }>('/v1/admin/secrets', undefined, session)
    setKeys(result.keys)
  }

  useEffect(() => {
    void action.run(refresh)
    const timer = window.setInterval(() => {
      void refresh().catch(() => {})
    }, 5000)
    return () => window.clearInterval(timer)
  }, [session])

  function exportFile() {
    if (!window.confirm('导出的文件包含明文凭据，请自行妥善保管。继续导出？')) return
    void action.run(async () => {
      const file = await api('/v1/admin/secrets/export', {}, session)
      const url = URL.createObjectURL(new Blob([JSON.stringify(file, null, 2)], { type: 'application/json' }))
      const anchor = document.createElement('a')
      anchor.href = url
      anchor.download = `trustd-credentials-${new Date().toISOString().slice(0, 10)}.json`
      anchor.click()
      window.setTimeout(() => URL.revokeObjectURL(url), 1000)
    })
  }

  function importFile(event: ChangeEvent<HTMLInputElement>) {
    const file = event.currentTarget.files?.[0]
    event.currentTarget.value = ''
    if (!file) return
    void action.run(async () => {
      if (file.size > 8 * 1024 * 1024) throw new Error('凭据文件不能超过 8 MiB。')
      const data = JSON.parse(await file.text())
      if (
        data?.format !== 'localshelld-credentials' ||
        data?.version !== 1 ||
        !data?.credentials ||
        Array.isArray(data.credentials) ||
        typeof data.credentials !== 'object'
      )
        throw new Error('不是受支持的 trustd 凭据文件。')
      if (
        !window.confirm(
          `用文件中的 ${Object.keys(data.credentials).length} 条凭据替换全部现有凭据？已连接服务时，也会替换云端凭据。`
        )
      )
        return false
      await api('/v1/admin/secrets/import', data, session)
      clearForm()
      await refresh()
    }, '凭据已导入。')
  }

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
      <SyncSettings session={session} onChanged={refresh} />
      <div className="secrets-transfer">
        <Button onClick={() => fileInput.current?.click()} disabled={action.busy}>
          导入文件
        </Button>
        <Button onClick={exportFile} disabled={action.busy || keys === null}>
          导出文件
        </Button>
        <input
          ref={fileInput}
          className="secrets-file-input"
          type="file"
          accept="application/json,.json"
          onChange={importFile}
          aria-label="选择凭据文件"
        />
      </div>
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
            type="text"
            value={value}
            onChange={(event) => setValue(event.currentTarget.value)}
            autoComplete="off"
            spellCheck={false}
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
