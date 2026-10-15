import { useEffect, useState } from 'react'
import { Alert, Button, Checkbox, Form, Input, InputNumber, Modal, Progress, Select, Space, Typography, message } from 'antd'
import type { SqlConnSpec, SqlDbType, SqlRequest, SqlResult } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { describeSqlError } from './sqlTypes'

export interface SqlConnFormValues {
  type: SqlDbType
  host: string
  port: number
  user: string
  password: string
  database: string
  sqlitePath: string
  name: string
}

const TYPE_LABELS: Record<SqlDbType, string> = {
  mysql: 'MySQL',
  mariadb: 'MariaDB',
  postgres: 'PostgreSQL',
  sqlite: 'SQLite',
}

const DEFAULT_PORTS: Record<Exclude<SqlDbType, 'sqlite'>, number> = {
  mysql: 3306,
  mariadb: 3306,
  postgres: 5432,
}

function defaultValues(type: SqlDbType = 'mysql'): SqlConnFormValues {
  return {
    type,
    host: '127.0.0.1',
    port: type === 'sqlite' ? 3306 : DEFAULT_PORTS[type],
    user: 'root',
    password: '',
    database: '',
    sqlitePath: '',
    name: TYPE_LABELS[type],
  }
}

interface DriverProgress {
  stage: string
  percent: number
  transferred: number
  total: number
}

/** 新建连接弹窗：直连表单 + 驱动进度（sql-progress）+ 错误提示 + 保存连接。
 *  保存的连接落 sql-connections.db（后端 sql-conns 帧），跨启动可用。 */
export function SqlConnModal({
  sessionId,
  open,
  request,
  initialValues,
  onClose,
  onConnected,
}: {
  sessionId: string
  open: boolean
  request: (op: SqlRequest, timeoutMs?: number) => Promise<SqlResult>
  initialValues?: Partial<SqlConnFormValues>
  onClose: () => void
  onConnected: (r: SqlResult, spec: SqlConnSpec) => void
}) {
  const [form, setForm] = useState<SqlConnFormValues>(defaultValues())
  const [connecting, setConnecting] = useState(false)
  const [testing, setTesting] = useState(false)
  const [saveConn, setSaveConn] = useState(true)
  const [progress, setProgress] = useState<DriverProgress>()
  const [error, setError] = useState<{ title: string; hint?: string }>()
  const [testOk, setTestOk] = useState<string>()

  // 每次打开重置为记忆值（缺省补默认值），连接失败不清空已填内容
  useEffect(() => {
    if (open) {
      setForm({ ...defaultValues(initialValues?.type ?? 'mysql'), ...initialValues })
      setProgress(undefined)
      setError(undefined)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  // 表单任意字段变化后，上一次测试结果不再可信，自动清除
  useEffect(() => {
    setTestOk(undefined)
  }, [form])

  // 驱动启动进度（connect 期间后端推送 sql-progress）
  useEffect(() => {
    const off = agentSocket.onControl((frame) => {
      if (frame.t !== 'sql-progress' || frame.s !== sessionId) return
      const m = frame.m
      if (!m.stage) return
      const total = m.total ?? 0
      setProgress({
        stage: m.stage,
        transferred: m.transferred ?? 0,
        total,
        percent: total > 0 ? Math.floor(((m.transferred ?? 0) / total) * 100) : 0,
      })
    })
    return off
  }, [sessionId])

  const setType = (type: SqlDbType) => {
    setForm((f) => ({
      ...f,
      type,
      port: type === 'sqlite' ? f.port : DEFAULT_PORTS[type],
    }))
  }

  // 本地 .db 文件选择（Tauri 原生对话框；浏览器环境降级为手填路径）
  const browseSqlite = async () => {
    try {
      const { open } = await import('@tauri-apps/plugin-dialog')
      const picked = await open({
        title: '选择 SQLite 数据库文件',
        filters: [{ name: 'SQLite 数据库', extensions: ['db', 'sqlite', 'sqlite3'] }],
        multiple: false,
      })
      if (typeof picked === 'string') {
        setForm((f) => ({ ...f, sqlitePath: picked }))
      }
    } catch {
      message.warning('当前环境不支持文件选择，请手动输入路径')
    }
  }

  /** 从表单构建连接规格；必填项缺失时展示错误并返回 null */
  const buildSpec = (): SqlConnSpec | null => {
    const spec: SqlConnSpec = {
      type: form.type,
      host: form.host || undefined,
      port: form.port || undefined,
      user: form.user || undefined,
      password: form.password || undefined,
      database: form.database || undefined,
      sqlitePath: form.type === 'sqlite' ? form.sqlitePath || undefined : undefined,
      name: form.name || undefined,
    }
    if (form.type === 'sqlite' && !spec.sqlitePath) {
      setError({ title: '请选择本地 SQLite 文件' })
      return null
    }
    if (form.type !== 'sqlite' && !form.host) {
      setError({ title: '请填写数据库地址' })
      return null
    }
    return spec
  }

  /** 测试连接：connect 成功即视为可达，展示服务器版本后立即断开清理，不落连接列表 */
  const doTest = async () => {
    const spec = buildSpec()
    if (!spec) return
    setTesting(true)
    setError(undefined)
    setTestOk(undefined)
    setProgress(undefined)
    try {
      const r = await request({ op: 'connect', conn: spec }, 180000)
      if (r.ok) {
        setTestOk(`连接成功${r.serverVersion ? `（${r.serverVersion}）` : ''}`)
        // 测试连接不保留：断开清理（失败不影响测试结论）
        if (r.connId) {
          try {
            await request({ op: 'disconnect', connId: r.connId }, 15000)
          } catch {
            /* 清理失败可忽略 */
          }
        }
      } else {
        setError(describeSqlError(r.errorKind, r.message))
      }
    } finally {
      setTesting(false)
      setProgress(undefined)
    }
  }

  const doConnect = async () => {
    const spec = buildSpec()
    if (!spec) return
    setConnecting(true)
    setError(undefined)
    setProgress(undefined)
    try {
      // 直连：驱动内置安装包，宽超时兜底网络握手
      const r = await request({ op: 'connect', conn: spec }, 180000)
      if (r.ok) {
        message.success(`连接成功${r.serverVersion ? `（${r.serverVersion}）` : ''}`)
        if (saveConn) {
          agentSocket.sendControl({ t: 'sql-conns', m: { op: 'create', conn: {
            name: form.name || TYPE_LABELS[form.type],
            type: form.type,
            host: form.type === 'sqlite' ? '' : form.host,
            port: form.type === 'sqlite' ? 0 : form.port,
            user: form.type === 'sqlite' ? '' : form.user,
            password: form.type === 'sqlite' ? '' : form.password,
            database: form.type === 'sqlite' ? '' : form.database,
            sqlitePath: form.type === 'sqlite' ? form.sqlitePath : '',
          } } })
        }
        onConnected(r, spec)
        onClose()
      } else {
        setError(describeSqlError(r.errorKind, r.message))
      }
    } finally {
      setConnecting(false)
      setProgress(undefined)
    }
  }

  return (
    <Modal
      title="新建数据库连接"
      open={open}
      onCancel={onClose}
      footer={[
        <Button key="test" onClick={() => void doTest()} loading={testing} disabled={connecting}>
          测试连接
        </Button>,
        <Button key="cancel" onClick={onClose} disabled={connecting || testing}>
          取消
        </Button>,
        <Button key="connect" type="primary" onClick={() => void doConnect()} loading={connecting} disabled={testing}>
          连接
        </Button>,
      ]}
      width={460}
      destroyOnHidden
    >
      <Form layout="vertical" size="small">
        {error && (
          <Alert
            type="error"
            showIcon
            style={{ marginBottom: 12 }}
            message={error.title}
            description={error.hint && <span style={{ whiteSpace: 'pre-wrap' }}>{error.hint}</span>}
          />
        )}
        {testOk && !error && (
          <Alert type="success" showIcon style={{ marginBottom: 12 }} message={testOk} />
        )}
        <Form.Item label="数据库类型" required>
          <Select<SqlDbType>
            value={form.type}
            onChange={setType}
            options={(Object.keys(TYPE_LABELS) as SqlDbType[]).map((t) => ({
              value: t,
              label: TYPE_LABELS[t],
            }))}
          />
        </Form.Item>
        {form.type === 'sqlite' ? (
          <Form.Item label="本地 SQLite 文件" required>
            <Space.Compact style={{ width: '100%' }}>
              <Input
                value={form.sqlitePath}
                onChange={(e) => setForm((f) => ({ ...f, sqlitePath: e.target.value }))}
                placeholder="D:\data\app.db"
              />
              <Button onClick={() => void browseSqlite()}>浏览…</Button>
            </Space.Compact>
          </Form.Item>
        ) : (
          <>
            <div style={{ display: 'flex', gap: 8 }}>
              <Form.Item label="主机" style={{ flex: 1, marginBottom: 12 }}>
                <Input
                  value={form.host}
                  onChange={(e) => setForm((f) => ({ ...f, host: e.target.value }))}
                  placeholder="127.0.0.1"
                />
              </Form.Item>
              <Form.Item label="端口" style={{ width: 110, marginBottom: 12 }}>
                <InputNumber
                  value={form.port}
                  min={1}
                  max={65535}
                  style={{ width: '100%' }}
                  onChange={(v) => setForm((f) => ({ ...f, port: v ?? 0 }))}
                />
              </Form.Item>
            </div>
            <Form.Item label="用户名">
              <Input
                value={form.user}
                onChange={(e) => setForm((f) => ({ ...f, user: e.target.value }))}
                placeholder="root"
              />
            </Form.Item>
            <Form.Item label="密码">
              <Input.Password
                value={form.password}
                onChange={(e) => setForm((f) => ({ ...f, password: e.target.value }))}
                autoComplete="new-password"
              />
            </Form.Item>
            <Form.Item label="默认数据库">
              <Input
                value={form.database}
                onChange={(e) => setForm((f) => ({ ...f, database: e.target.value }))}
                placeholder="可留空"
              />
            </Form.Item>
          </>
        )}
        <Form.Item label="连接名" style={{ marginBottom: 8 }}>
          <Input
            value={form.name}
            onChange={(e) => setForm((f) => ({ ...f, name: e.target.value }))}
            placeholder={`${TYPE_LABELS[form.type]} 连接`}
          />
        </Form.Item>
        <Checkbox checked={saveConn} onChange={(e) => setSaveConn(e.target.checked)}>
          连接成功后保存到连接列表
        </Checkbox>
      </Form>
      {progress && (
        <div>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            {progress.stage}
          </Typography.Text>
          <Progress
            percent={progress.percent}
            status="active"
            size="small"
            format={(p) =>
              progress.total > 0 ? `${p}%（${progress.transferred}/${progress.total} 字节）` : '进行中'
            }
          />
        </div>
      )}
    </Modal>
  )
}
