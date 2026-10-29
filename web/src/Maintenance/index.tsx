import { type FormEvent, useEffect, useState } from 'react'

import { Button, CakeProvider, Field, TextBox } from '@a1knla/cakeui'

import { Feedback } from '../Feedback'
import { api } from '../api'

import './index.scss'

type Agent = { id: string; name: string; path: string; detected: boolean }
type AgentResult = { name: string; path: string; backup: string | null; error: string | null }
type Job = {
  running: boolean
  stage: string
  error: string | null
  completed: 'install' | 'uninstall' | null
  agents: AgentResult[]
}
type Info = {
  version: string
  platform: string
  program: string
  system_config: string
  user_config: string
  registered: boolean
  registered_executable: string | null
  registered_config: string | null
  system_running: boolean
  user_initialized: boolean
  shell: string
  shell_available: boolean
  elevation_available: boolean
  user_url: string
  system_url: string
  agents: Agent[]
  job: Job
}

const steps = ['检查安装', '安装选项', '安装服务', '完成']
const platforms: Record<string, string> = { windows: 'Windows', macos: 'macOS', linux: 'Linux' }
const stages: Record<string, string> = {
  preparing: '准备安装文件',
  elevation: '等待系统授权并安装服务',
  user_service: '启动用户服务并验证安装',
  agents: '配置 AI agent',
  failed: '安装未完成',
  done: '操作完成',
}

export function Maintenance({ token }: { token: string }) {
  const [info, setInfo] = useState<Info | null>(null)
  const [step, setStep] = useState(0)
  const [password, setPassword] = useState('')
  const [confirmation, setConfirmation] = useState('')
  const [error, setError] = useState('')
  const [sending, setSending] = useState(false)
  const [closed, setClosed] = useState(false)
  const [selectedAgents, setSelectedAgents] = useState<string[]>([])
  const [replaceAgents, setReplaceAgents] = useState(false)
  const busy = sending || !!info?.job.running
  const needsPassword = !!info && (!info.registered || !info.user_initialized)

  async function refresh(signal?: AbortSignal) {
    try {
      const next = await api<Info>('/setup/v1/status', undefined, token, signal)
      setInfo(next)
      setError('')
    } catch (cause) {
      if (!signal?.aborted) setError(cause instanceof Error ? cause.message : '无法读取安装状态。')
    }
  }

  useEffect(() => {
    if (!token || closed) return
    const controller = new AbortController()
    void refresh(controller.signal)
    const interval = window.setInterval(() => void refresh(controller.signal), 2000)
    return () => {
      controller.abort()
      window.clearInterval(interval)
    }
  }, [token, closed])

  useEffect(() => {
    if (info?.job.running || info?.job.error) setStep(2)
    if (info?.job.completed) setStep(3)
  }, [info?.job.stage])

  async function begin(operation: 'install' | 'uninstall') {
    setSending(true)
    setError('')
    try {
      await api(
        `/setup/v1/${operation}`,
        operation === 'install'
          ? { password: needsPassword ? password : null, agents: selectedAgents, replace_agents: replaceAgents }
          : {},
        token
      )
      setPassword('')
      setConfirmation('')
      setStep(2)
      await refresh()
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : '无法开始操作。')
    } finally {
      setSending(false)
    }
  }

  function install(event: FormEvent) {
    event.preventDefault()
    if (password !== confirmation) {
      setError('两次输入的密码不一致。')
      return
    }
    void begin('install')
  }

  function uninstall() {
    if (window.confirm('停止并移除系统服务和当前用户的自启动？程序、配置和凭据会保留。')) void begin('uninstall')
  }

  async function finish() {
    try {
      await api('/setup/v1/close', {}, token)
      setClosed(true)
      setInfo(null)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : '无法关闭向导。')
    }
  }

  return (
    <CakeProvider theme="blue" mode="light" accentColor="#245aa8" className="app-theme">
      <main className="maintenance">
        <header className="maintenance-header">
          <div className="maintenance-brand">
            <h1 className="maintenance-name">trustd</h1>
            <span className="maintenance-caption">安装与维护</span>
          </div>
          {info && (
            <span className="maintenance-version">
              {platforms[info.platform] ?? info.platform} · {info.version}
            </span>
          )}
        </header>
        <div className="maintenance-panel">
          <aside className="maintenance-sidebar" aria-label="安装步骤">
            <ol className="maintenance-steps">
              {steps.map((title, index) => (
                <li
                  className="maintenance-step"
                  key={title}
                  aria-current={step === index ? 'step' : undefined}
                  data-complete={step > index}
                >
                  <span className="maintenance-step-number">{step > index ? '✓' : index + 1}</span>
                  {title}
                </li>
              ))}
            </ol>
            <p className="maintenance-scope">
              本机安装
              <br />
              系统服务与用户服务
            </p>
          </aside>
          <section className="maintenance-content" aria-live="polite">
            {closed ? (
              <>
                <h2 className="section-title">向导已关闭</h2>
                <p className="description">可以关闭此页面。</p>
              </>
            ) : !token ? (
              <>
                <h2 className="section-title">重新打开向导</h2>
                <p className="description">此链接已失效。请重新启动 trustd 安装与维护。</p>
              </>
            ) : !info ? (
              <>
                <h2 className="section-title">检查本机安装</h2>
                <p className="description">正在读取服务和安装目录。</p>
              </>
            ) : step === 0 ? (
              <>
                <h2 className="section-title">{info.registered ? '已检测到安装' : '安装 trustd'}</h2>
                <dl className="maintenance-checks">
                  <div className="maintenance-check">
                    <dt className="maintenance-check-label">系统服务</dt>
                    <dd className="maintenance-check-value">
                      {info.registered ? (info.system_running ? '运行中' : '已安装，未运行') : '未安装'}
                    </dd>
                  </div>
                  <div className="maintenance-check">
                    <dt className="maintenance-check-label">用户配置</dt>
                    <dd className="maintenance-check-value">{info.user_initialized ? '已初始化' : '待初始化'}</dd>
                  </div>
                  <div className="maintenance-check">
                    <dt className="maintenance-check-label">{info.platform === 'windows' ? 'PowerShell 7' : 'Bash'}</dt>
                    <dd className="maintenance-check-value" data-error={!info.shell_available}>
                      {info.shell_available ? '已就绪' : '未找到系统级安装'}
                    </dd>
                  </div>
                </dl>
                <h3 className="subsection-title">安装位置</h3>
                <dl className="maintenance-paths">
                  <div className="maintenance-path">
                    <dt className="maintenance-path-label">程序</dt>
                    <dd className="maintenance-path-value">{info.program}</dd>
                  </div>
                  <div className="maintenance-path">
                    <dt className="maintenance-path-label">系统配置</dt>
                    <dd className="maintenance-path-value">{info.system_config}</dd>
                  </div>
                  <div className="maintenance-path">
                    <dt className="maintenance-path-label">用户配置</dt>
                    <dd className="maintenance-path-value">{info.user_config}</dd>
                  </div>
                </dl>
                {info.registered_config && info.registered_config !== info.system_config && (
                  <p className="maintenance-note">旧位置的系统配置会迁移并备份。已有密码、配对和凭据保持不变。</p>
                )}
                {!info.shell_available && (
                  <p className="maintenance-note">
                    请先安装{' '}
                    <a
                      className="content-link"
                      href="https://learn.microsoft.com/powershell/scripting/install/installing-powershell-on-windows"
                      target="_blank"
                      rel="noreferrer"
                    >
                      PowerShell 7 的系统级版本
                    </a>
                    ，再刷新检查。
                  </p>
                )}
                {!info.elevation_available && (
                  <Feedback message="缺少系统提权工具。Linux 请先安装 polkit/pkexec。" error />
                )}
                <footer className="maintenance-actions">
                  {info.registered && (
                    <Button variant="ghost" onClick={uninstall} disabled={busy}>
                      卸载服务
                    </Button>
                  )}
                  <Button onClick={() => void refresh()} disabled={busy}>
                    刷新检查
                  </Button>
                  <Button
                    variant="primary"
                    disabled={busy || !info.shell_available || !info.elevation_available}
                    onClick={() => setStep(1)}
                  >
                    下一步
                  </Button>
                </footer>
              </>
            ) : step === 1 ? (
              <form className="maintenance-form" onSubmit={install}>
                <h2 className="section-title">{needsPassword ? '初始化管理密码' : '修复或更新安装'}</h2>
                {needsPassword ? (
                  <>
                    <p className="description">此密码用于初始化尚未配置的管理端。已有配置不会重置。</p>
                    <Field htmlFor="setup-password" label="主密码">
                      <TextBox
                        id="setup-password"
                        type="text"
                        autoComplete="off"
                        maxLength={4096}
                        value={password}
                        onChange={(event) => setPassword(event.currentTarget.value)}
                      />
                    </Field>
                    <Field htmlFor="setup-confirmation" label="确认主密码">
                      <TextBox
                        id="setup-confirmation"
                        type="text"
                        autoComplete="off"
                        maxLength={4096}
                        value={confirmation}
                        onChange={(event) => setConfirmation(event.currentTarget.value)}
                      />
                    </Field>
                  </>
                ) : (
                  <p className="description">
                    使用当前版本重新安装服务、迁移旧目录并修复目录权限。管理密码和凭据保持不变。
                  </p>
                )}
                <fieldset className="maintenance-agent-options" disabled={busy}>
                  <legend className="maintenance-agent-legend">自动配置 AI agent</legend>
                  <p className="maintenance-agent-description">选择需要接入的客户端，也可稍后手动配置。</p>
                  <div className="maintenance-agent-list">
                    {info.agents.map((agent) => (
                      <label className="maintenance-agent" key={agent.id}>
                        <input
                          className="maintenance-agent-checkbox"
                          type="checkbox"
                          checked={selectedAgents.includes(agent.id)}
                          onChange={(event) => {
                            const checked = event.currentTarget.checked
                            setSelectedAgents((current) =>
                              checked ? [...current, agent.id] : current.filter((id) => id !== agent.id)
                            )
                          }}
                        />
                        <span className="maintenance-agent-info">
                          <span className="maintenance-agent-title">
                            <span className="maintenance-agent-name">{agent.name}</span>
                            {agent.detected && <span className="maintenance-agent-detected">已找到配置</span>}
                          </span>
                          <span className="maintenance-agent-path">{agent.path}</span>
                        </span>
                      </label>
                    ))}
                  </div>
                  {selectedAgents.length > 0 && (
                    <label className="maintenance-agent-replace">
                      <input
                        className="maintenance-agent-checkbox"
                        type="checkbox"
                        checked={replaceAgents}
                        onChange={(event) => setReplaceAgents(event.currentTarget.checked)}
                      />
                      替换所选客户端已有的 trustd 配置，保留备份
                    </label>
                  )}
                </fieldset>
                <dl className="maintenance-checks">
                  <div className="maintenance-check">
                    <dt className="maintenance-check-label">安装版本</dt>
                    <dd className="maintenance-check-value">{info.version}</dd>
                  </div>
                  <div className="maintenance-check">
                    <dt className="maintenance-check-label">系统服务身份</dt>
                    <dd className="maintenance-check-value">{info.platform === 'windows' ? 'SYSTEM' : 'root'}</dd>
                  </div>
                  <div className="maintenance-check">
                    <dt className="maintenance-check-label">用户服务身份</dt>
                    <dd className="maintenance-check-value">当前登录用户</dd>
                  </div>
                </dl>
                <p className="maintenance-note">安装需要系统授权，现有服务会短暂重启。</p>
                <footer className="maintenance-actions">
                  <Button onClick={() => setStep(0)} disabled={busy}>
                    上一步
                  </Button>
                  <Button type="submit" variant="primary" disabled={busy}>
                    {info.registered ? '安装并修复' : '开始安装'}
                  </Button>
                </footer>
              </form>
            ) : step === 2 ? (
              <>
                <h2 className="section-title">{info.job.error ? '操作未完成' : '安装与维护'}</h2>
                <div className="maintenance-progress" role="status">
                  <span className="maintenance-progress-mark" data-running={busy}>
                    {info.job.error ? '!' : '…'}
                  </span>
                  {stages[info.job.stage] ?? '准备操作'}
                </div>
                {info.job.error && (
                  <>
                    <Feedback message={info.job.error} error />
                    <p className="maintenance-note">修复安装可重复执行。已有配置和迁移备份会保留。</p>
                    <footer className="maintenance-actions">
                      <Button onClick={() => setStep(0)}>返回检查</Button>
                    </footer>
                  </>
                )}
                {busy && <p className="maintenance-note">请在系统授权窗口中确认。完成后会检查两个服务的运行状态。</p>}
              </>
            ) : (
              <>
                <h2 className="section-title">{info.job.completed === 'uninstall' ? '服务已卸载' : '安装完成'}</h2>
                {info.job.completed === 'uninstall' ? (
                  <p className="description">程序、配置和凭据已保留，可通过此向导重新安装。</p>
                ) : (
                  <>
                    <p className="description">系统服务和用户服务已启动。</p>
                    {info.job.agents.length > 0 && (
                      <section className="maintenance-agent-results" aria-label="AI agent 配置结果">
                        <h3 className="subsection-title">AI agent 配置</h3>
                        {info.job.agents.map((agent) => (
                          <div className="maintenance-agent-result" key={agent.path}>
                            <div className="maintenance-agent-title">
                              <span className="maintenance-agent-name">{agent.name}</span>
                              <span className="maintenance-agent-status" data-error={!!agent.error}>
                                {agent.error ? '未配置' : '已配置'}
                              </span>
                            </div>
                            {agent.error && <Feedback message={agent.error} error />}
                            <details className="maintenance-agent-details">
                              <summary className="maintenance-agent-summary">
                                配置文件{agent.backup ? '与备份' : ''}
                              </summary>
                              <p className="maintenance-agent-path">{agent.path}</p>
                              {agent.backup && <p className="maintenance-agent-path">备份：{agent.backup}</p>}
                            </details>
                          </div>
                        ))}
                        <p className="maintenance-note">
                          重启已配置的客户端以加载 trustd。未完成的配置可返回安装选项重试。
                        </p>
                      </section>
                    )}
                    <div className="maintenance-links">
                      <a className="maintenance-management-link" href={info.user_url} target="_blank" rel="noreferrer">
                        打开用户管理
                      </a>
                      <a
                        className="maintenance-management-link"
                        href={info.system_url}
                        target="_blank"
                        rel="noreferrer"
                      >
                        打开系统管理
                      </a>
                    </div>
                    <p className="maintenance-note">
                      首次安装：在用户管理的桌面授权中生成配对码，再到系统管理完成配对。已有配对无需重复设置。
                    </p>
                    <p className="maintenance-note">
                      {info.platform === 'windows'
                        ? '开始菜单'
                        : info.platform === 'macos'
                          ? '用户 Applications 目录'
                          : '应用菜单'}
                      中已添加维护入口。
                    </p>
                  </>
                )}
                <footer className="maintenance-actions">
                  <Button onClick={() => setStep(0)}>返回检查</Button>
                  <Button variant="primary" onClick={() => void finish()}>
                    完成
                  </Button>
                </footer>
              </>
            )}
            {error && !closed && <Feedback message={error} error />}
          </section>
        </div>
      </main>
    </CakeProvider>
  )
}
