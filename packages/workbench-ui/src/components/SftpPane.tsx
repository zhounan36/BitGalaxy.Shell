import { useCallback, useEffect, useRef, useState } from 'react'
import type { MouseEvent as ReactMouseEvent, DragEvent as ReactDragEvent, HTMLAttributes, Key } from 'react'
import {
  Breadcrumb,
  Button,
  Empty,
  Input,
  Menu,
  Modal,
  Progress,
  Skeleton,
  Space,
  Table,
  Tag,
  Tooltip,
  message,
} from 'antd'
import type { MenuProps } from 'antd'
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
import { writeClipboardText } from '../api/clipboard'
import { formatBytes, formatTime } from '../utils/format'

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

/** 拖出下载（DownloadURL → Agent 本地 HTTP 端点）仅桌面壳 / 本机宿主可用：
 *  云端工作台宿主直连 127.0.0.1 不可达 */
const canDragOut =
  isTauri ||
  (typeof location !== 'undefined' && (location.hostname === 'localhost' || location.hostname === '127.0.0.1'))

/** 面板内拖动记录（dragstart 写入；dragstart 内不能 await，故拖出 URL 也走预取） */
interface DragPayload {
  sessionId: string
  path: string
  names: string[]
}

/** 右键菜单状态：行菜单带 entry（或选区 entries），空白区菜单都不带 */
interface MenuState {
  x: number
  y: number
  entry?: SftpEntry
  entries?: SftpEntry[]
}

type RowProps = HTMLAttributes<HTMLElement> & { 'data-drop-dir'?: string }

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

/** Windows 非法文件名字符与控制字符清洗（DownloadURL 的本地文件名字段） */
const sanitizeName = (name: string) =>
  [...name].map((ch) => (ch < ' ' || '<>:"/\\|?*'.includes(ch) ? '_' : ch)).join('')

/** 右键菜单条目（按命中对象分组；isTauri 专属项在浏览器宿主隐藏） */
function ctxMenuItems(menu: MenuState): MenuProps['items'] {
  const e = menu.entry
  if (menu.entries && menu.entries.length > 0) {
    return [
      { key: 'batch-dl', label: '批量下载…' },
      { key: 'batch-del', label: '批量删除', danger: true },
      { type: 'divider' },
      { key: 'copy-name', label: '复制文件名' },
      { key: 'copy-path', label: '复制完整路径' },
    ]
  }
  if (!e) {
    return [
      { key: 'upload', label: '上传文件…' },
      ...(isTauri ? [{ key: 'upload-dir', label: '上传文件夹…' }] : []),
      { key: 'mkdir', label: '新建文件夹' },
      { key: 'refresh', label: '刷新' },
    ]
  }
  if (e.type === 'dir') {
    return [
      { key: 'open', label: '打开' },
      ...(isTauri ? [{ key: 'dl-dir', label: '下载文件夹…' }] : []),
      { key: 'copy-path', label: '复制完整路径' },
      { type: 'divider' },
      { key: 'rename', label: '重命名' },
      { key: 'delete', label: '删除', danger: true },
    ]
  }
  return [
    { key: 'dl', label: '下载到本地…' },
    { key: 'copy-name', label: '复制文件名' },
    { key: 'copy-path', label: '复制完整路径' },
    { type: 'divider' },
    { key: 'rename', label: '重命名' },
    { key: 'delete', label: '删除', danger: true },
  ]
}

/** SFTP 文件管理面板：目录浏览 + 桌面化交互（拖入上传 / 拖出下载 / 面板内移动 /
 *  右键菜单 / 多选批量）+ 传输（断点续传、文件夹）。Tauri 壳内走原生传输
 * （Agent 直传本地磁盘），浏览器回退 WS 流式。 */
export function SftpPane({ sessionId }: { sessionId: string }) {
  const [path, setPath] = useState<string>()
  const [entries, setEntries] = useState<SftpEntry[]>()
  const [loading, setLoading] = useState(false)
  const [opened, setOpened] = useState(false)
  const [error, setError] = useState<string>()
  const [transfers, setTransfers] = useState<TransferView[]>([])
  const [dlBase, setDlBase] = useState<string>()
  const [selected, setSelected] = useState<string[]>([])
  const [menu, setMenu] = useState<MenuState>()
  const [dragOver, setDragOver] = useState(false)
  const pendingRef = useRef(new Map<string, { resolve: (r: SftpResult) => void; timer: number }>())
  const doneWaitersRef = useRef(new Map<string, (ok: boolean, message?: string, errorKind?: string) => void>())
  const fileInputRef = useRef<HTMLInputElement>(null)
  /** dlBase 是否已发起过请求（失败不留痕，下次 ls 重试） */
  const dlAskedRef = useRef(false)
  /** 面板内拖动记录（ref 存放，避免频繁拖动引发重渲染） */
  const draggingRef = useRef<DragPayload | undefined>(undefined)
  /** Tauri 内部拖动当前高亮的目标行/面包屑项 */
  const hoverDropDirRef = useRef<HTMLElement | undefined>(undefined)

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

  // pathRef：事件回调里取最新目录（不进 effect 依赖，避免反复注册）
  const pathRef = useRef<string | undefined>(undefined)

  const ls = useCallback(
    async (p: string) => {
      setLoading(true)
      try {
        const r = await request({ op: 'ls', path: p })
        if (r.ok && r.entries) {
          setPath(r.resolvedPath)
          pathRef.current = r.resolvedPath
          setEntries(r.entries)
          setError(undefined)
          setSelected([])
          // 拖出下载令牌按会话签发，拖出前缓存（dragstart 内不能 await）
          if (canDragOut && !dlAskedRef.current) {
            dlAskedRef.current = true
            void request({ op: 'dl-open' }).then((d) => {
              if (d.ok && d.dlBase) setDlBase(d.dlBase)
              else dlAskedRef.current = false
            })
          }
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
          doneWaitersRef.current.get(m.opId)?.(m.ok, m.message, m.errorKind)
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

  /** Tauri 拖入上传：本地绝对路径 stat 后区分文件/目录 → put-file/put-dir（目录递归上传） */
  const uploadLocalPaths = async (paths: string[]) => {
    if (!path) return
    const { invoke } = await import('@tauri-apps/api/core')
    const items: { path: string; name: string; sizeB: number; isDir: boolean; target: string }[] = []
    for (const p of paths) {
      try {
        const s = await invoke<{ name: string; size: number; is_dir: boolean }>('stat_file', { path: p })
        items.push({ path: p, name: s.name, sizeB: s.is_dir ? 0 : s.size, isDir: s.is_dir, target: join(path, s.name) })
      } catch (err) {
        message.error(`无法读取 ${p}：${err instanceof Error ? err.message : String(err)}`)
      }
    }
    if (items.length === 0) return
    const decisions = await preflightBatch(sessionId, 'upload', items)
    if (!decisions) return
    for (let i = 0; i < items.length; i++) {
      const d = decisions[i]
      if (d.skip) continue
      startTransfer(
        items[i].isDir
          ? { op: 'put-dir', path: items[i].path, to: d.target, fresh: d.fresh }
          : { op: 'put-file', path: items[i].path, to: d.target, fresh: d.fresh },
        items[i].name,
        'up',
      )
    }
    void ls(path)
  }

  /** 浏览器回退：HTML5 File 经 0x10 数据帧分块上传（统一预检+确认后执行） */
  const uploadFiles = async (files: File[]) => {
    if (!path || files.length === 0) return
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
    const done = await new Promise<{ ok: boolean; message?: string; errorKind?: string }>(
      (resolve) => {
        doneWaitersRef.current.set(key, (ok, message, errorKind) => resolve({ ok, message, errorKind }))
        // 大文件兜底超时（20 分钟）
        window.setTimeout(() => resolve({ ok: false, message: '下载超时（20 分钟无进展）' }), 20 * 60 * 1000)
      },
    )
    off()
    doneWaitersRef.current.delete(key)
    agentSocket.sendControl({
      t: 'sftp',
      s: sessionId,
      m: { op: 'download-close', opId: key, rid: crypto.randomUUID() },
    })
    // 打开/读取失败（无读权限、连接断开）：立即展示失败原因，不能静默装成功
    if (!done.ok) {
      showTransferError(`下载 ${entry.name} 失败`, done.errorKind, done.message)
      setTransfers((prev) =>
        prev.map((t) => (t.key === key ? { ...t, status: 'error', message: done.message ?? '下载失败' } : t)),
      )
      return
    }
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

  /** 面板内移动（尽力而为）：同目录移动提示无效；逐项 rename 后汇总 */
  const moveEntries = (fromDir: string, names: string[], toDir: string) => {
    if (fromDir === toDir) {
      message.warning('目标目录与源目录相同')
      return
    }
    Modal.confirm({
      title: `将 ${names.length} 项移动到「${toDir}」？`,
      content: names.slice(0, 5).join('、') + (names.length > 5 ? ' 等' : ''),
      okText: '移动',
      onOk: async () => {
        let ok = 0
        const failed: string[] = []
        for (const name of names) {
          const r = await request({ op: 'rename', from: join(fromDir, name), to: join(toDir, name) })
          if (r.ok) ok++
          else failed.push(`${name}（${r.message ?? '失败'}）`)
        }
        if (failed.length === 0) message.success(`已移动 ${ok} 项到 ${toDir}`)
        else message.warning(`移动完成：成功 ${ok} 项，失败 ${failed.length} 项——${failed.join('、')}`)
        setSelected([])
        const p = pathRef.current
        if (p) void ls(p)
      },
    })
  }

  /** 批量下载：Tauri 选本地目录后 get-file/get-dir；浏览器宿主串行流式下载（回退体验） */
  const batchDownload = async () => {
    if (!path || selected.length === 0) return
    const targets = (entries ?? []).filter((e) => selected.includes(e.name))
    if (targets.length === 0) return
    if (!isTauri) {
      for (const e of targets) await download(e)
      return
    }
    const { open } = await import('@tauri-apps/plugin-dialog')
    const dir = await open({ directory: true, multiple: false })
    if (!dir || typeof dir !== 'string') return
    const sep = dir.match(/\\/) ? '\\' : '/'
    const base = dir.replace(/[\\/]+$/, '')
    const items = targets.map((e) => ({
      path: join(path, e.name),
      name: e.name,
      sizeB: e.sizeB,
      target: base + sep + e.name,
    }))
    const decisions = await preflightBatch(sessionId, 'download', items)
    if (!decisions) return
    for (let i = 0; i < items.length; i++) {
      const d = decisions[i]
      if (d.skip) continue
      const e = targets[i]
      startTransfer(
        e.type === 'dir'
          ? { op: 'get-dir', path: items[i].path, to: d.target, fresh: d.fresh }
          : { op: 'get-file', path: items[i].path, to: d.target, fresh: d.fresh },
        e.name,
        'down',
      )
    }
  }

  /** 批量删除：汇总确认后逐项 rm */
  const batchDelete = () => {
    if (!path || selected.length === 0) return
    const targets = (entries ?? []).filter((e) => selected.includes(e.name))
    if (targets.length === 0) return
    const dirCount = targets.filter((e) => e.type === 'dir').length
    Modal.confirm({
      title: `删除 ${targets.length} 项（${targets.length - dirCount} 个文件、${dirCount} 个文件夹）？`,
      content: dirCount > 0 ? '文件夹将连同其中全部内容一起删除' : undefined,
      okText: '删除',
      okButtonProps: { danger: true },
      onOk: async () => {
        let ok = 0
        const failed: string[] = []
        for (const e of targets) {
          const r = await request({ op: 'rm', path: join(path, e.name), recursive: e.type === 'dir' })
          if (r.ok) ok++
          else failed.push(`${e.name}（${r.message ?? '失败'}）`)
        }
        if (failed.length === 0) message.success(`已删除 ${ok} 项`)
        else message.warning(`删除完成：成功 ${ok} 项，失败 ${failed.length} 项——${failed.join('、')}`)
        setSelected([])
        void ls(path)
      },
    })
  }

  // ── 拖出（DownloadURL）+ 面板内拖动（dragstart 记录来源）──────────────────

  const onDragStartRow = (entry: SftpEntry, e: ReactDragEvent) => {
    if (!path) return
    // 命中行在选区 → 拖整个选区，否则单行
    const names = selected.includes(entry.name) ? selected : [entry.name]
    const payload: DragPayload = { sessionId, path, names }
    draggingRef.current = payload
    e.dataTransfer.effectAllowed = 'copy'
    // 浏览器宿主内部移动通道（Tauri 下 wry 拦截 HTML5 拖放，走 onDragDropEvent 的 draggingRef）
    e.dataTransfer.setData('application/x-bgshell-remote', JSON.stringify(payload))
    e.dataTransfer.setData('text/plain', entry.name)
    // 拖出仅支持单文件（DownloadURL 协议限制）；目录/多选走右键批量下载
    if (canDragOut && dlBase && entry.type === 'file' && names.length === 1) {
      const safeName = sanitizeName(entry.name)
      const dlUrl = `${dlBase}?p=${encodeURIComponent(join(path, entry.name))}&n=${encodeURIComponent(safeName)}`
      e.dataTransfer.setData('DownloadURL', `application/octet-stream:${safeName}:${dlUrl}`)
    }
  }

  /** 浏览器宿主：目标目录（文件夹行/面包屑）接受面板内拖动 */
  const onDropDirOver = (e: ReactDragEvent) => {
    if (e.dataTransfer.types.includes('application/x-bgshell-remote')) {
      e.preventDefault()
      e.stopPropagation()
      e.dataTransfer.dropEffect = 'copy'
    }
  }

  const onDropDirDrop = (toDir: string) => (e: ReactDragEvent) => {
    const raw = e.dataTransfer.getData('application/x-bgshell-remote')
    // 非内部拖动（OS 文件）：交由根 onDrop 走上传
    if (!raw) return
    e.preventDefault()
    e.stopPropagation()
    try {
      const info = JSON.parse(raw) as DragPayload
      if (info.sessionId !== sessionId) return
      moveEntries(info.path, info.names, toDir)
    } catch {
      // 脏数据静默忽略
    }
  }

  // ── 右键菜单 / 双击 ─────────────────────────────────────────────────────

  const onRowContextMenu = (entry: SftpEntry, e: ReactMouseEvent) => {
    e.preventDefault()
    e.stopPropagation()
    if (menu) {
      setMenu(undefined)
      return
    }
    if (selected.includes(entry.name) && selected.length > 1) {
      setMenu({ x: e.clientX, y: e.clientY, entries: (entries ?? []).filter((x) => selected.includes(x.name)) })
      return
    }
    setSelected([entry.name])
    setMenu({ x: e.clientX, y: e.clientY, entry })
  }

  const onRootContextMenu = (e: ReactMouseEvent) => {
    const t = e.target as HTMLElement | null
    if (t?.closest('tr[data-row-key]')) return // 行菜单已处理
    e.preventDefault()
    if (menu) {
      setMenu(undefined)
      return
    }
    setMenu({ x: e.clientX, y: e.clientY })
  }

  const onMenuClick: MenuProps['onClick'] = (info) => {
    const m = menu
    setMenu(undefined)
    if (!m) return
    const entry = m.entry
    const targets = m.entries ?? (entry ? [entry] : [])
    const names = targets.map((x) => x.name)
    switch (info.key) {
      case 'open':
        if (entry && path) void ls(join(path, entry.name))
        break
      case 'dl':
        if (entry) void download(entry)
        break
      case 'dl-dir':
        if (entry) void downloadDir(entry)
        break
      case 'copy-name':
        void writeClipboardText(names.join('\n'))
        break
      case 'copy-path':
        if (path) void writeClipboardText(names.map((n) => join(path, n)).join('\n'))
        break
      case 'rename':
        if (entry) doRename(entry)
        break
      case 'delete':
        if (entry) {
          Modal.confirm({
            title: `删除「${entry.name}」？`,
            content: entry.type === 'dir' ? '文件夹将连同其中全部内容一起删除' : undefined,
            okText: '删除',
            okButtonProps: { danger: true },
            onOk: () => doDelete(entry),
          })
        }
        break
      case 'upload':
        if (isTauri) void pickAndUpload(false)
        else fileInputRef.current?.click()
        break
      case 'upload-dir':
        void pickAndUpload(true)
        break
      case 'mkdir':
        doMkdir()
        break
      case 'refresh':
        if (path) void ls(path)
        break
      case 'batch-dl':
        void batchDownload()
        break
      case 'batch-del':
        batchDelete()
        break
    }
  }

  // 菜单关闭：左键点外部 / Escape（右键按下不关，交给 contextmenu 判定）
  useEffect(() => {
    if (!menu) return
    const onDown = (e: MouseEvent) => {
      if (e.button === 2) return
      if ((e.target as HTMLElement | null)?.closest('.sftp-ctx-menu')) return
      setMenu(undefined)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMenu(undefined)
    }
    window.addEventListener('mousedown', onDown)
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('mousedown', onDown)
      window.removeEventListener('keydown', onKey)
    }
  }, [menu])

  // ── 行交互（拖出/拖入目标/右键/双击）────────────────────────────────────

  const onRow = (entry: SftpEntry): RowProps => ({
    draggable: true,
    'data-drop-dir': entry.type === 'dir' && path ? join(path, entry.name) : undefined,
    onDragStart: (e: ReactDragEvent) => onDragStartRow(entry, e),
    onDragEnd: () => {
      draggingRef.current = undefined
    },
    onDoubleClick: () => {
      if (!path) return
      if (entry.type === 'dir') void ls(join(path, entry.name))
      else void download(entry)
    },
    onContextMenu: (e: ReactMouseEvent) => onRowContextMenu(entry, e),
    onDragOver: onDropDirOver,
    onDrop: onDropDirDrop(entry.type === 'dir' && path ? join(path, entry.name) : ''),
  })

  // ── Tauri OS 级拖放（HTML5 拖放被 wry 拦截）：命中本面板才处理 ────────────
  // 回调经 ref 取最新逻辑，effect 只随 sessionId 注册一次
  const actionsRef = useRef({ uploadLocalPaths, moveEntries })
  actionsRef.current = { uploadLocalPaths, moveEntries }

  useEffect(() => {
    if (!isTauri) return
    let disposed = false
    let unlisten: (() => void) | undefined
    // over 事件不带 paths，OS 文件拖入与否由 enter 时判定并沿用
    let osDrag = false
    const clearRowHover = () => {
      hoverDropDirRef.current?.classList.remove('sftp-row-drop-hover')
      hoverDropDirRef.current = undefined
    }
    void (async () => {
      const { getCurrentWebview } = await import('@tauri-apps/api/webview')
      if (disposed) return
      unlisten = await getCurrentWebview().onDragDropEvent((ev) => {
        const p = ev.payload
        if (p.type === 'enter') osDrag = p.paths.length > 0
        if (p.type === 'leave') {
          setDragOver(false)
          clearRowHover()
          return
        }
        const scale = window.devicePixelRatio || 1
        const el = document.elementFromPoint(p.position.x / scale, p.position.y / scale) as HTMLElement | null
        if (!el?.closest(`[data-sftp-session="${sessionId}"]`)) {
          setDragOver(false)
          clearRowHover()
          return
        }
        if (p.type === 'drop') {
          osDrag = false
          setDragOver(false)
          clearRowHover()
          if (p.paths.length > 0) {
            void actionsRef.current.uploadLocalPaths(p.paths)
            return
          }
          // 面板内拖动（wry 不转发则自然失效，无副作用）
          const drag = draggingRef.current
          draggingRef.current = undefined
          const toDir = el.closest('[data-drop-dir]')?.getAttribute('data-drop-dir') ?? undefined
          if (drag && toDir) void actionsRef.current.moveEntries(drag.path, drag.names, toDir)
          return
        }
        // enter / over
        if (osDrag) {
          setDragOver(true)
          clearRowHover()
          return
        }
        if (!draggingRef.current) return
        const target = el.closest('[data-drop-dir]') as HTMLElement | null
        if (target !== hoverDropDirRef.current) {
          clearRowHover()
          if (target) {
            hoverDropDirRef.current = target
            target.classList.add('sftp-row-drop-hover')
          }
        }
      })
    })()
    return () => {
      disposed = true
      unlisten?.()
      setDragOver(false)
      hoverDropDirRef.current?.classList.remove('sftp-row-drop-hover')
      hoverDropDirRef.current = undefined
    }
  }, [sessionId])

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
  ]

  // 菜单浮层防出屏（同终端菜单的钳制策略）
  const menuStyle = menu
    ? {
        left: Math.max(8, Math.min(menu.x, window.innerWidth - 220)),
        top: Math.max(8, Math.min(menu.y, window.innerHeight - 320)),
      }
    : undefined

  return (
    <div
      className={`sftp-pane${dragOver ? ' drag-over' : ''}`}
      data-sftp-session={sessionId}
      style={{ height: '100%', overflow: 'auto', background: 'var(--bg-deep)', padding: 12 }}
      onDragOver={(e) => e.preventDefault()}
      onDrop={(e) => {
        e.preventDefault()
        // 面板内拖动落到空白处 = 无效目标；OS 文件走上传
        if (e.dataTransfer.getData('application/x-bgshell-remote')) return
        void uploadFiles([...e.dataTransfer.files])
      }}
      onContextMenu={onRootContextMenu}
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
        <Tag style={{ marginLeft: 'auto' }}>拖入上传 · 拖出下载到目标文件夹 · 右键更多操作</Tag>
      </Space>

      {selected.length > 0 && (
        <div className="sftp-selbar">
          <span>已选 {selected.length} 项</span>
          <Button size="small" onClick={() => void batchDownload()}>
            批量下载
          </Button>
          <Button size="small" danger onClick={batchDelete}>
            批量删除
          </Button>
          <Button size="small" onClick={() => setSelected([])}>
            取消选择
          </Button>
        </div>
      )}

      {path && (
        <Breadcrumb
          style={{ marginBottom: 8 }}
          items={[
            {
              title: (
                <a data-drop-dir="/" onDragOver={onDropDirOver} onDrop={onDropDirDrop('/')} onClick={() => void ls('/')}>
                  /
                </a>
              ),
            },
            ...crumbs.map((c, i) => {
              const p = '/' + crumbs.slice(0, i + 1).join('/')
              return {
                title: (
                  <a data-drop-dir={p} onDragOver={onDropDirOver} onDrop={onDropDirDrop(p)} onClick={() => void ls(p)}>
                    {c}
                  </a>
                ),
              }
            }),
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
          rowSelection={{
            selectedRowKeys: selected as Key[],
            onChange: (keys) => setSelected(keys as string[]),
          }}
          onRow={onRow}
          locale={{
            emptyText: <Empty description={error ?? (path ? '目录为空' : '连接主机中…')} />,
          }}
        />
      )}

      {menu && (
        <div
          className="sftp-ctx-menu"
          style={menuStyle}
          onContextMenu={(e) => {
            e.preventDefault()
            e.stopPropagation()
          }}
        >
          <Menu items={ctxMenuItems(menu)} onClick={onMenuClick} />
        </div>
      )}
    </div>
  )
}