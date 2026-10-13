import { useCallback, useEffect, useRef, useState } from 'react'
import {
  Breadcrumb,
  Button,
  Empty,
  Input,
  Modal,
  Popconfirm,
  Progress,
  Skeleton,
  Space,
  Table,
  Tag,
  Tooltip,
  message,
} from 'antd'
import {
  ArrowUpOutlined,
  CloseOutlined,
  FileOutlined,
  FolderAddOutlined,
  FolderOpenOutlined,
  FolderOutlined,
  LinkOutlined,
  ReloadOutlined,
  UploadOutlined,
} from '@ant-design/icons'
import type { SftpEntry, SftpRequest, SftpResult } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { preflightBatch, showTransferError } from '../api/transferService'
import { formatBytes, formatTime } from '../utils/format'

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

interface TransferView {
  key: string
  /** 展示名（任务名；目录任务运行中显示当前文件） */
  name: string
  dir: 'up' | 'down'
  transferred: number
  total: number
  status: 'active' | 'done' | 'error'
  speedBps?: number
  fileDone?: number
  fileTotal?: number
  current?: string
  message?: string
}

/** SFTP 文件管理面板：目录浏览 / 上传(含拖拽、文件夹) / 下载(断点续传、文件夹) / 新建/重命名/删除。
 * Tauri 壳内走原生传输（Agent 直传本地磁盘 + 断点续传），浏览器回退 WS 流式。 */
export function SftpPane({ sessionId }: { sessionId: string }) {
  const [path, setPath] = useState<string>()
  const [entries, setEntries] = useState<SftpEntry[]>()
  const [loading, setLoading] = useState(false)
  const [opened, setOpened] = useState(false)
  const [error, setError] = useState<string>()
  const [transfers, setTransfers] = useState<TransferView[]>([])
  const pendingRef = useRef(new Map<string, { resolve: (r: SftpResult) => void; timer: number }>())
  const doneWaitersRef = useRef(new Map<string, () => void>())
  const fileInputRef = useRef<HTMLInputElement>(null)

  /** 请求/响应关联（rid）+ 超时兜底 */
  const request = useCallback(
    async (op: SftpRequest, timeoutMs = 20000): Promise<SftpResult> => {
      const rid = crypto.randomUUID()
      return await new Promise<SftpResult>((resolve) => {
        const timer = window.setTimeout(() => {
          if (pendingRef.current.delete(rid)) resolve({ op: op.op, rid, ok: false, message: '请求超时' })
        }, timeoutMs)
        pendingRef.current.set(rid, { resolve, timer })
        agentSocket.sendControl({ t: 'sftp', s: sessionId, m: { ...op, rid } })
      })
    },
    [sessionId],
  )

  const ls = useCallback(
    async (p: string) => {
      setLoading(true)
      try {
        const r = await request({ op: 'ls', path: p })
        if (r.ok && r.entries) {
          setPath(r.resolvedPath)
          setEntries(r.entries)
          setError(undefined)
        } else if (!r.ok) {
          setError(r.message ?? '读取目录失败')
          message.error(r.message ?? '读取目录失败')
        }
      } finally {
        setLoading(false)
      }
    },
    [request],
  )

  // 订阅结果/进度/下载完成/原生传输结束信号
  useEffect(() => {
    const off = agentSocket.onControl((frame) => {
      if (frame.t === 'session-opened' && frame.s === sessionId) setOpened(true)
      if (frame.t === 'error' && frame.s === sessionId) setError(frame.m.message)
      if (frame.t === 'sftp-result' && frame.s === sessionId) {
        const { m } = frame
        if (m.rid) {
          const pending = pendingRef.current.get(m.rid)
          if (pending) {
            clearTimeout(pending.timer)
            pendingRef.current.delete(m.rid)
            pending.resolve(m)
          }
        }
        if (m.op === 'download-done' && m.opId) {
          doneWaitersRef.current.get(m.opId)?.()
        }
        // 原生传输任务结束（get-*/put-*）
        if (m.op === 'transfer-done' && m.opId) {
          const opId = m.opId
          if (m.ok) {
            setTransfers((prev) => prev.filter((t) => t.key !== opId))
            message.success(`传输完成（${formatBytes(m.sizeB ?? 0)}）`)
          } else {
            showTransferError('传输失败', m.errorKind, m.message)
            setTransfers((prev) =>
              prev.map((t) =>
                t.key === opId
                  ? { ...t, status: 'error', message: m.message ?? '传输失败' }
                  : t,
              ),
            )
          }
        }
      }
      if (frame.t === 'sftp-progress' && frame.s === sessionId) {
        const p = frame.m
        setTransfers((prev) =>
          prev.map((t) =>
            t.key === p.opId
              ? {
                  ...t,
                  transferred: p.transferred,
                  total: p.total,
                  speedBps: p.speedBps,
                  fileDone: p.fileDone,
                  fileTotal: p.fileTotal,
                  current: p.name,
                }
              : t,
          ),
        )
      }
    })
    return off
  }, [sessionId])

  // 首个 ls 等会话就绪：session-opened 在 SSH 建连（含指纹确认）并挂载 SFTP 子系统后才发出，
  // 提前请求会被后端以"SFTP 会话未就绪"拒绝
  useEffect(() => {
    if (opened) void ls('.')
  }, [opened, ls])

  const join = (dir: string, name: string) => (dir.endsWith('/') ? `${dir}${name}` : `${dir}/${name}`)
  const parent = (p: string) => {
    const parts = p.split('/').filter(Boolean)
    return parts.length <= 1 ? '/' : `/${parts.slice(0, -1).join('/')}`
  }

  /** 发起原生传输任务（get-file/get-dir/put-file/put-dir）：立即建传输条目，进度/结束由帧驱动 */
  const startTransfer = (
    op: Extract<SftpRequest, { op: 'get-file' | 'get-dir' | 'put-file' | 'put-dir' }>,
    label: string,
    dir: 'up' | 'down',
  ) => {
    void (async () => {
      const r = await request(op)
      if (!r.ok || !r.opId) {
        showTransferError('启动传输失败', r.errorKind, r.message)
        return
      }
      setTransfers((prev) => [
        ...prev,
        { key: r.opId!, name: label, dir, transferred: 0, total: 0, status: 'active' },
      ])
    })()
  }

  const cancelTransfer = (key: string) => {
    setTransfers((prev) => prev.filter((t) => t.key !== key))
    void request({ op: 'transfer-cancel', opId: key })
  }

  const doMkdir = () => {
    let value = ''
    Modal.confirm({
      title: '新建文件夹',
      content: <Input placeholder="文件夹名称" onChange={(e) => (value = e.target.value)} />,
      onOk: async () => {
        if (!value || !path) return
        const r = await request({ op: 'mkdir', path: join(path, value) })
        if (r.ok) void ls(path)
        else message.error(r.message ?? '新建失败')
      },
    })
  }

  const doRename = (entry: SftpEntry) => {
    let value = entry.name
    Modal.confirm({
      title: `重命名「${entry.name}」`,
      content: <Input defaultValue={entry.name} onChange={(e) => (value = e.target.value)} />,
      onOk: async () => {
        if (!value || !path || value === entry.name) return
        const r = await request({ op: 'rename', from: join(path, entry.name), to: join(path, value) })
        if (r.ok) void ls(path)
        else message.error(r.message ?? '重命名失败')
      },
    })
  }

  const doDelete = async (entry: SftpEntry) => {
    if (!path) return
    const r = await request({
      op: 'rm',
      path: join(path, entry.name),
      recursive: entry.type === 'dir',
    })
    if (r.ok) {
      message.success(`已删除 ${entry.name}`)
      void ls(path)
    } else {
      message.error(r.message ?? '删除失败')
    }
  }

  /** Tauri 上传：原生选择器取本地路径 → 统一预检+确认 → put-file/put-dir（Agent 直读本地文件，断点续传） */
  const pickAndUpload = async (folder: boolean) => {
    if (!path) return
    const { open } = await import('@tauri-apps/plugin-dialog')
    const picked = await open(
      folder ? { directory: true, multiple: false } : { multiple: true },
    )
    const paths = Array.isArray(picked) ? picked : picked ? [picked] : []
    if (paths.length === 0) return
    const items = paths.map((p) => {
      const name = p.split(/[\\/]/).pop() ?? p
      return { path: p, name, sizeB: 0, target: join(path, name) }
    })
    const decisions = await preflightBatch(sessionId, 'upload', items)
    if (!decisions) return
    for (let i = 0; i < items.length; i++) {
      const d = decisions[i]
      if (d.skip) continue
      startTransfer(
        folder
          ? { op: 'put-dir', path: items[i].path, to: d.target, fresh: d.fresh }
          : { op: 'put-file', path: items[i].path, to: d.target, fresh: d.fresh },
        items[i].name,
        'up',
      )
    }
  }

  /** 浏览器回退：HTML5 File 经 0x10 数据帧分块上传（统一预检+确认后执行） */
  const uploadFiles = async (files: File[]) => {
    if (!path) return
    const items = files.map((f) => ({ file: f, name: f.name, sizeB: f.size, target: join(path, f.name) }))
    const decisions = await preflightBatch(sessionId, 'upload', items)
    if (!decisions) return
    for (let i = 0; i < items.length; i++) {
      const { file } = items[i]
      const d = decisions[i]
      if (d.skip) continue
      const key = crypto.randomUUID()
      setTransfers((prev) => [...prev, { key, name: file.name, dir: 'up', transferred: 0, total: file.size, status: 'active' }])
      const open = await request({ op: 'upload-open', path: d.target })
      if (!open.ok || !open.opId) {
        showTransferError(`${file.name} 上传失败`, open.errorKind, open.message)
        setTransfers((prev) => prev.filter((t) => t.key !== key))
        continue
      }
      const reader = file.stream().getReader()
      for (;;) {
        const { done, value } = await reader.read()
        if (done) break
        if (value) {
          await agentSocket.sendSftpChunk(sessionId, open.opId, value)
          setTransfers((prev) =>
            prev.map((t) =>
              t.key === key ? { ...t, transferred: t.transferred + value.byteLength } : t,
            ),
          )
        }
      }
      const close = await request({ op: 'upload-close', opId: open.opId }, 10 * 60 * 1000)
      if (close.ok) message.success(`${file.name} 上传完成`)
      else showTransferError(`${file.name} 上传失败`, close.errorKind, close.message)
      setTransfers((prev) => prev.filter((t) => t.key !== key))
    }
    void ls(path)
  }

  /** 下载：Tauri 下原生保存对话框 + 统一预检 + get-file（断点续传）；浏览器回退 WS 流式 Blob */
  const download = async (entry: SftpEntry) => {
    if (!path) return
    const remotePath = join(path, entry.name)
    if (isTauri) {
      const { save } = await import('@tauri-apps/plugin-dialog')
      const local = await save({ defaultPath: entry.name })
      if (!local) return
      const decisions = await preflightBatch(sessionId, 'download', [
        { path: remotePath, name: entry.name, sizeB: entry.sizeB, target: local },
      ])
      const d = decisions?.[0]
      if (!d || d.skip) return
      startTransfer({ op: 'get-file', path: remotePath, to: d.target, fresh: d.fresh }, entry.name, 'down')
      return
    }
    const open = await request({ op: 'download-open', path: join(path, entry.name) })
    if (!open.ok || !open.opId) {
      message.error(open.message ?? '打开下载失败')
      return
    }
    const key = open.opId
    const total = open.sizeB ?? entry.sizeB
    setTransfers((prev) => [...prev, { key, name: entry.name, dir: 'down', transferred: 0, total, status: 'active' }])

    const chunks: Uint8Array[] = []
    let received = 0
    const off = agentSocket.registerDownloadChunk(key, (chunk) => {
      chunks.push(chunk)
      received += chunk.byteLength
      setTransfers((prev) => prev.map((t) => (t.key === key ? { ...t, transferred: received } : t)))
    })
    await new Promise<void>((resolve) => {
      doneWaitersRef.current.set(key, resolve)
      // 大文件兜底超时（20 分钟）
      window.setTimeout(resolve, 20 * 60 * 1000)
    })
    off()
    doneWaitersRef.current.delete(key)
    agentSocket.sendControl({
      t: 'sftp',
      s: sessionId,
      m: { op: 'download-close', opId: key, rid: crypto.randomUUID() },
    })

    const blob = new Blob(chunks as unknown as BlobPart[])
    const a = document.createElement('a')
    a.href = URL.createObjectURL(blob)
    a.download = entry.name
    a.click()
    URL.revokeObjectURL(a.href)
    setTransfers((prev) => prev.filter((t) => t.key !== key))
    message.success(`已下载 ${entry.name}（${formatBytes(blob.size)}）`)
  }

  /** 文件夹下载：选择本地目录 → 统一预检 + 递归下载（含断点续传，重复下载自动跳过已完成文件） */
  const downloadDir = async (entry: SftpEntry) => {
    if (!path) return
    const { open } = await import('@tauri-apps/plugin-dialog')
    const dir = await open({ directory: true, multiple: false })
    if (!dir || typeof dir !== 'string') return
    const sep = dir.match(/\\/) ? '\\' : '/'
    const local = dir.replace(/[\\/]+$/, '') + sep + entry.name
    const decisions = await preflightBatch(sessionId, 'download', [
      { path: join(path, entry.name), name: entry.name, sizeB: entry.sizeB, target: local },
    ])
    const d = decisions?.[0]
    if (!d || d.skip) return
    startTransfer({ op: 'get-dir', path: join(path, entry.name), to: d.target, fresh: d.fresh }, entry.name, 'down')
  }

  const crumbs = (path ?? '').split('/').filter(Boolean)

  const columns = [
    {
      title: '名称',
      dataIndex: 'name',
      render: (_: unknown, e: SftpEntry) => (
        <Space>
          {e.type === 'dir' ? <FolderOutlined /> : e.type === 'symlink' ? <LinkOutlined /> : <FileOutlined />}
          <a
            onClick={() => {
              if (e.type === 'dir' && path) void ls(join(path, e.name))
            }}
            style={e.type === 'dir' ? {} : { color: 'inherit', cursor: 'default' }}
          >
            {e.name}
          </a>
        </Space>
      ),
    },
    { title: '大小', dataIndex: 'sizeB', width: 110, render: (v: number, e: SftpEntry) => (e.type === 'dir' ? '—' : formatBytes(v)) },
    { title: '修改时间', dataIndex: 'mtimeMs', width: 180, render: (v: number) => formatTime(v) },
    {
      title: '操作',
      key: 'actions',
      width: 200,
      render: (_: unknown, e: SftpEntry) => (
        <Space size={4}>
          {e.type === 'dir' ? (
            isTauri ? (
              <Button type="link" size="small" onClick={() => void downloadDir(e)}>
                下载
              </Button>
            ) : null
          ) : (
            <Button type="link" size="small" onClick={() => void download(e)}>
              下载
            </Button>
          )}
          <Button type="link" size="small" onClick={() => doRename(e)}>
            重命名
          </Button>
          <Popconfirm title={`删除「${e.name}」？${e.type === 'dir' ? '（含全部内容）' : ''}`} onConfirm={() => void doDelete(e)}>
            <Button type="link" size="small" danger>
              删除
            </Button>
          </Popconfirm>
        </Space>
      ),
    },
  ]

  return (
    <div
      style={{ height: '100%', overflow: 'auto', background: 'var(--bg-deep)', padding: 12 }}
      onDragOver={(e) => e.preventDefault()}
      onDrop={(e) => {
        e.preventDefault()
        void uploadFiles([...e.dataTransfer.files])
      }}
    >
      <Space wrap style={{ marginBottom: 8 }}>
        <Tooltip title="返回上级">
          <Button icon={<ArrowUpOutlined />} disabled={!path || path === '/'} onClick={() => path && void ls(parent(path))} />
        </Tooltip>
        <Button icon={<ReloadOutlined />} onClick={() => path && void ls(path)} />
        <Button icon={<FolderAddOutlined />} onClick={doMkdir}>
          新建文件夹
        </Button>
        <Button type="primary" icon={<UploadOutlined />} onClick={() => (isTauri ? void pickAndUpload(false) : fileInputRef.current?.click())}>
          上传
        </Button>
        {isTauri && (
          <Tooltip title="递归上传整个文件夹（断点续传）">
            <Button icon={<FolderOpenOutlined />} onClick={() => void pickAndUpload(true)}>
              上传文件夹
            </Button>
          </Tooltip>
        )}
        <input
          ref={fileInputRef}
          type="file"
          multiple
          hidden
          onChange={(e) => {
            void uploadFiles([...(e.target.files ?? [])])
            e.target.value = ''
          }}
        />
        <Tag style={{ marginLeft: 'auto' }}>拖拽文件到此区域即可上传</Tag>
      </Space>

      {path && (
        <Breadcrumb
          style={{ marginBottom: 8 }}
          items={[
            { title: <a onClick={() => void ls('/')}>/</a> },
            ...crumbs.map((c, i) => ({
              title: <a onClick={() => void ls('/' + crumbs.slice(0, i + 1).join('/'))}>{c}</a>,
            })),
          ]}
        />
      )}

      {transfers.length > 0 && (
        <div style={{ marginBottom: 8, display: 'flex', flexDirection: 'column', gap: 4 }}>
          {transfers.map((t) => {
            const showFiles = t.fileTotal !== undefined && t.fileTotal > 0
            return (
              <div key={t.key} style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                <span style={{ width: 200, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }} title={showFiles && t.current ? `${t.name}：${t.current}` : t.name}>
                  {t.dir === 'up' ? '↑' : '↓'} {t.name}
                  {showFiles && `（${t.fileDone ?? 0}/${t.fileTotal}）`}
                </span>
                <Progress
                  style={{ flex: 1, marginBottom: 0 }}
                  size="small"
                  status={t.status === 'error' ? 'exception' : t.status === 'done' ? 'success' : 'active'}
                  percent={t.total > 0 ? Math.floor((t.transferred / t.total) * 100) : 0}
                  format={() =>
                    t.total > 0
                      ? `${formatBytes(t.transferred)} / ${formatBytes(t.total)}${t.speedBps ? ` · ${formatBytes(t.speedBps)}/s` : ''}`
                      : '准备中…'
                  }
                />
                {t.status === 'error' ? (
                  <Tooltip title={t.message}>
                    <Button type="text" size="small" danger icon={<CloseOutlined />} onClick={() => setTransfers((prev) => prev.filter((x) => x.key !== t.key))} />
                  </Tooltip>
                ) : (
                  <Button type="text" size="small" icon={<CloseOutlined />} onClick={() => cancelTransfer(t.key)} />
                )}
              </div>
            )
          })}
        </div>
      )}

      {loading && !entries ? (
        <Skeleton active paragraph={{ rows: 10 }} />
      ) : (
        <Table
          size="small"
          rowKey="name"
          columns={columns}
          dataSource={entries}
          loading={loading}
          pagination={false}
          locale={{
            emptyText: <Empty description={error ?? (path ? '目录为空' : '连接主机中…')} />,
          }}
        />
      )}
    </div>
  )
}
