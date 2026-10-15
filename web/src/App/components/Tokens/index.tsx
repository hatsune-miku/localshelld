import { type FormEvent, useEffect, useState } from 'react'

import {
  Button,
  ComboBox,
  Field,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TextArea,
} from '@a1knla/cakeui'

import { Feedback } from '../../../Feedback'
import { type TokenRecord, api, authorizationText, dateTime } from '../../../api'
import { useAction } from '../../../useAction'

import './index.scss'

export function Tokens({ session, scope }: { session: string; scope: string }) {
  const [duration, setDuration] = useState('86400')
  const [issued, setIssued] = useState<{ token: string; record: TokenRecord } | null>(null)
  const [records, setRecords] = useState<TokenRecord[]>([])
  const action = useAction()
  async function refresh() {
    setRecords(await api<TokenRecord[]>('/v1/admin/tokens/list', {}, session))
  }
  useEffect(() => {
    void action.run(refresh)
  }, [session])
  function issue(event: FormEvent) {
    event.preventDefault()
    void action.run(async () => {
      setIssued(
        await api(
          '/v1/admin/tokens/issue',
          { permanent: duration === 'permanent', ttl_seconds: duration === 'permanent' ? null : Number(duration) },
          session
        )
      )
      await refresh()
    }, '令牌已签发，可复制授权文本。')
  }
  async function revoke(id: string) {
    if (!window.confirm('撤销令牌并立即终止它的所有会话？')) return false
    await api('/v1/admin/tokens/revoke', { id }, session)
    if (issued?.record.id === id) setIssued(null)
    await refresh()
  }
  return (
    <div className="tokens-view">
      <h2 className="section-title">签发访问令牌</h2>
      <p className="description">授权范围：{scope}。令牌仅在本次服务运行期间有效，可随时撤销。</p>
      <form className="form-stack" onSubmit={issue}>
        <Field htmlFor="token-duration" label="有效期">
          <ComboBox id="token-duration" value={duration} onChange={(event) => setDuration(event.currentTarget.value)}>
            <option value="86400">24 小时</option>
            <option value="3600">1 小时</option>
            <option value="900">15 分钟</option>
            <option value="604800">7 天</option>
            <option value="permanent">直到撤销或服务重启</option>
          </ComboBox>
        </Field>
        <div className="form-actions">
          <Button type="submit" variant="primary" loading={action.busy}>
            确认授权并签发
          </Button>
        </div>
      </form>
      {issued && (
        <div className="issued-token">
          <Field htmlFor="issued-token" label="本次访问令牌">
            <TextArea id="issued-token" className="credential-value" value={issued.token} readOnly rows={2} />
          </Field>
          <div className="form-actions">
            <Button
              onClick={() =>
                void action.run(
                  () => navigator.clipboard.writeText(authorizationText(issued.token)),
                  '授权文本和令牌已复制。'
                )
              }
            >
              复制授权文本
            </Button>
            <Button
              variant="ghost"
              onClick={() => void action.run(() => navigator.clipboard.writeText(issued.token), '令牌已复制。')}
            >
              仅复制令牌
            </Button>
          </div>
          <details className="authorization-preview">
            <summary>查看复制的授权说明</summary>
            <p className="authorization-copy">{authorizationText(issued.token)}</p>
          </details>
        </div>
      )}
      <Feedback {...action} />
      <section className="token-records">
        <div className="token-records-heading">
          <h3 className="subsection-title">本次运行已签发令牌</h3>
          <Button variant="ghost" disabled={action.busy} onClick={() => void action.run(refresh, '列表已刷新。')}>
            刷新列表
          </Button>
        </div>
        {!records.length ? (
          <p className="empty-state">尚未签发令牌。</p>
        ) : (
          <div className="data-table-wrap">
            <Table className="data-table">
              <TableHead>
                <TableRow>
                  <TableHeader>标识 / 签发时间</TableHeader>
                  <TableHeader>到期时间</TableHeader>
                  <TableHeader>状态</TableHeader>
                  <TableHeader>操作</TableHeader>
                </TableRow>
              </TableHead>
              <TableBody>
                {records.map((record) => {
                  const expired = record.expires_at !== null && record.expires_at * 1000 <= Date.now()
                  return (
                    <TableRow key={record.id}>
                      <TableCell>
                        <span className="token-id" title={record.id}>
                          {record.id.slice(0, 8)}
                        </span>
                        <span className="token-date">{dateTime(record.issued_at)}</span>
                      </TableCell>
                      <TableCell>{dateTime(record.expires_at)}</TableCell>
                      <TableCell>{record.revoked ? '已撤销' : expired ? '已到期' : '有效'}</TableCell>
                      <TableCell>
                        <Button
                          size="small"
                          variant="ghost"
                          disabled={record.revoked || expired || action.busy}
                          onClick={() => void action.run(() => revoke(record.id), '令牌已撤销。')}
                        >
                          撤销
                        </Button>
                      </TableCell>
                    </TableRow>
                  )
                })}
              </TableBody>
            </Table>
          </div>
        )}
      </section>
    </div>
  )
}
