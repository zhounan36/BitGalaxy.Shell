/**
 * 终端拖拽上传：把本地文件经所属 SSH 会话（同连接上惰性打开的 SFTP 子系统）
 * 上传到该终端的当前目录。
 *
 * 当前目录来源：
 *  1. OSC 7（shell 集成上报，交互式 cd 实时准确）——由 TerminalPane 解析写入缓存
 *  2. sftp pwd op（独立 exec 通道，返回登录目录）——无 shell 集成时的兜底
 */
import type { SftpEntry } from '@bgshell/protocol'
import { agentSocket } from './ws'
import {
  preflightBatch,
  runNativeTransfer,
  sftpRequest,
  showTransferError,
  type NativeProgress,
  type TransferDecision,
} from './transferService'

const CHUNK = 256 * 1024

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

// ── 目录追踪（OSC 7 + 主机校验）与全屏程序检测 ───────────────────────────

/** 每会话目录状态：
 *  dir        最近一次被接受的 OSC 7 目录（undefined = 无可用上报）
 *  oscHost    最近一次 OSC 7 携带的主机名
 *  trustedHost 经本连接 exec hostname 得到的权威主机名（undefined = 未知，跳过校验）
 *  verifying  权威主机名获取进行中 */
interface SessionCwd {
  dir?: string
  oscHost?: string
  trustedHost?: string
  verifying?: boolean
}

const sessionCwds = new Map<string, SessionCwd>()

/** 全屏程序（top/vim/less 等交替屏幕）探针：sessionId → 是否处于交替屏幕 */
const altScreenProbes = new Map<string, () => boolean>()

export function clearSessionCwd(sid: string): void {
  sessionCwds.delete(sid)
  altScreenProbes.delete(sid)
}

/** 全屏程序检测：拖拽时读取（缓存目录=进入全屏前的提示符目录，恰好是期望目标） */
export function setSessionAltScreen(sid: string, probe: () => boolean): void {
  altScreenProbes.set(sid, probe)
}

export function isSessionAltScreen(sid: string): boolean {
  try {
    return altScreenProbes.get(sid)?.() ?? false
  } catch {
    return false
  }
}

/** 解析 OSC 7 payload（`file://host/path`，host 可能含 user@） */
export function parseOsc7(data: string): { host: string; dir: string } | null {
  const m = /^file:\/\/([^/]*)(\/.*)$/.exec(data.trim())
  if (!m) return null
  const safe = (s: string) => {
    try {
      return decodeURIComponent(s)
    } catch {
      return s
    }
  }
  // fish 等实现会带 user@host，主机比对只取 @ 后段
  const host = safe(m[1]).split('@').pop() ?? ''
  return { host, dir: safe(m[2]) }
}

/** 主机名比对（忽略大小写；短名/FQDN 互通；任一未知 → 放行，保持旧行为） */
function hostMatches(a: string | undefined, b: string | undefined): boolean {
  if (!a || !b) return true
  const x = a.toLowerCase()
  const y = b.toLowerCase()
  return x === y || x.startsWith(`${y}.`) || y.startsWith(`${x}.`)
}

/** OSC 7 上报：乐观接受，同时（一次性）拉取权威主机名校验——
 *  嵌套 SSH 内层 shell 的上报因主机不符被丢弃，避免传错主机 */
export function reportOsc7(sid: string, oscHost: string, dir: string): void {
  const st = sessionCwds.get(sid) ?? {}
  st.oscHost = oscHost
  st.dir = dir
  sessionCwds.set(sid, st)
  ensureTrustedHost(sid)
}

function ensureTrustedHost(sid: string): void {
  const st = sessionCwds.get(sid)
  if (!st || st.trustedHost !== undefined || st.verifying) return
  st.verifying = true
  void fetchHostAndDir(sid).then((r) => {
    const st = sessionCwds.get(sid)
    if (!st) return
    st.verifying = false
    if (!('error' in r) && r.host) st.trustedHost = r.host
    if (st.trustedHost !== undefined && !hostMatches(st.trustedHost, st.oscHost)) {
      st.dir = undefined // 校验失败：丢弃内层主机的目录
    }
  })
}

// ── SFTP 请求与原生传输：统一走 transferService ─────────────────────────

// sftpRequest / runNativeTransfer / NativeProgress 见 ./transferService

/** pwd op：登录目录 + 权威主机名（一次 exec 同时返回） */
async function fetchHostAndDir(
  sid: string,
): Promise<{ dir: string; host?: string } | { error: string }> {
  const r = await sftpRequest(sid, { op: 'pwd' })
  if (r.ok && r.resolvedPath) return { dir: r.resolvedPath, host: r.host }
  return { error: r.message ?? '无法获取远程目录' }
}

/** 终端会话的远端目录：通过主机校验的 OSC 7 优先，缺失时经 pwd op 兜底（登录目录） */
export async function resolveRemoteCwd(
  sid: string,
): Promise<{ dir: string; source: 'osc7' | 'login' } | { error: string }> {
  const st = sessionCwds.get(sid)
  if (st?.dir) return { dir: st.dir, source: 'osc7' }
  const r = await fetchHostAndDir(sid)
  if ('error' in r) return { error: r.error }
  // 顺手固化权威主机名，后续 OSC 7 校验免再查
  const st2 = sessionCwds.get(sid)
  if (st2 && r.host) st2.trustedHost = r.host
  return { dir: r.dir, source: 'login' }
}

// ── 上传 ─────────────────────────────────────────────────────────────────

/** 上传源：Tauri 原生拖放给本地绝对路径；浏览器 HTML5 拖放给 File 对象 */
export interface UploadSource {
  name: string
  sizeB: number
  path?: string
  file?: File
}

export interface UploadItemState {
  name: string
  transferred: number
  total: number
  status: 'uploading' | 'done' | 'error'
  message?: string
}

const join = (dir: string, name: string) => (dir.endsWith('/') ? `${dir}${name}` : `${dir}/${name}`)

async function readChunk(src: UploadSource, offset: number, len: number): Promise<Uint8Array> {
  if (src.path) {
    const { invoke } = await import('@tauri-apps/api/core')
    const buf = await invoke<ArrayBuffer>('read_file_chunk', { path: src.path, offset, len })
    return new Uint8Array(buf)
  }
  const buf = await src.file!.slice(offset, offset + len).arrayBuffer()
  return new Uint8Array(buf)
}

// ── 原生传输（Agent 直传本地磁盘 + 断点续传）：runNativeTransfer 见 transferService ──

/** 原生上传（put-file）：Agent 直读本地文件（断点续传），进度经 sftp-progress 帧回报。
 * fresh = 覆盖语义（跳过远端 APPEND，全新写入）。 */
async function putFileNative(
  sid: string,
  localPath: string,
  remotePath: string,
  label: string,
  fresh: boolean,
  onUpdate: (s: UploadItemState) => void,
): Promise<UploadItemState> {
  let last: UploadItemState = { name: label, transferred: 0, total: 0, status: 'uploading' }
  onUpdate(last)
  const r = await runNativeTransfer(
    sid,
    { op: 'put-file', path: localPath, to: remotePath, fresh },
    (p) => {
      last = { ...last, transferred: p.transferred, total: p.total }
      onUpdate(last)
    },
  )
  if (r.ok) return { ...last, transferred: r.sizeB ?? last.total, status: 'done' }
  showTransferError(`${label} 上传失败`, r.errorKind, r.message)
  return { ...last, status: 'error', message: r.message }
}

/**
 * 顺序上传多个文件到指定会话目录。
 * 先经统一预检（重名/权限/空间/盘格式）+ 确认交互（覆盖/重命名/取消），
 * 有本地路径（Tauri 原生拖放）→ put-file 原生直传（Agent 直读本地磁盘，断点续传）；
 * 浏览器 File 对象 → 0x10 数据帧分块（带 WS 背压）。
 * 通过 onProgress 回调推送每个文件的状态快照，返回最终快照。
 */
export async function uploadToSession(
  sid: string,
  dir: string,
  sources: UploadSource[],
  onProgress: (items: UploadItemState[]) => void,
): Promise<UploadItemState[]> {
  const items: UploadItemState[] = sources.map((s) => ({
    name: s.name,
    transferred: 0,
    total: s.sizeB,
    status: 'uploading',
  }))
  const emit = () => onProgress(items.map((i) => ({ ...i })))
  emit()

  // 统一预检 + 确认（覆盖 / 重命名 / 取消；null = 整体取消）
  const decisions = await preflightBatch(
    sid,
    'upload',
    sources.map((s) => ({ path: s.path, file: s.file, name: s.name, sizeB: s.sizeB, target: join(dir, s.name) })),
  )
  if (decisions === null) {
    const cancelled = items.map((i) => ({ ...i, status: 'error' as const, message: '已取消' }))
    onProgress(cancelled)
    return cancelled
  }

  for (let idx = 0; idx < sources.length; idx++) {
    const src = sources[idx]
    const d: TransferDecision = decisions[idx]
    if (d.skip) {
      items[idx] = { ...items[idx], status: 'error', message: '已跳过' }
      emit()
      continue
    }
    const remotePath = d.target

    if (src.path) {
      const fin = await putFileNative(sid, src.path, remotePath, src.name, d.fresh, (s) => {
        items[idx] = s
        emit()
      })
      items[idx] = fin
      emit()
      continue
    }

    const open = await sftpRequest(sid, { op: 'upload-open', path: remotePath })
    if (!open.ok || !open.opId) {
      showTransferError(`${src.name} 上传失败`, open.errorKind, open.message)
      items[idx] = { ...items[idx], status: 'error', message: open.message ?? '打开远程文件失败' }
      emit()
      continue
    }
    try {
      for (let offset = 0; offset < src.sizeB; offset += CHUNK) {
        const buf = await readChunk(src, offset, Math.min(CHUNK, src.sizeB - offset))
        await agentSocket.sendSftpChunk(sid, open.opId, buf)
        items[idx] = { ...items[idx], transferred: Math.min(offset + buf.byteLength, src.sizeB) }
        emit()
      }
    } catch (err) {
      items[idx] = {
        ...items[idx],
        status: 'error',
        message: err instanceof Error ? err.message : String(err),
      }
      emit()
      // 收尾要排空后端写入队列并等待服务器确认关闭，不能受默认 20s 超时影响
      await sftpRequest(sid, { op: 'upload-close', opId: open.opId }, 10 * 60 * 1000)
      continue
    }
    // close 需排空全部已入队块并等待服务器确认句柄关闭，大文件可能远超 20s
    const close = await sftpRequest(sid, { op: 'upload-close', opId: open.opId }, 10 * 60 * 1000)
    items[idx] =
      close.ok
        ? { ...items[idx], transferred: src.sizeB, status: 'done' }
        : { ...items[idx], status: 'error', message: close.message ?? '收尾失败' }
    emit()
  }
  return items.map((i) => ({ ...i }))
}

// ── 终端选区下载（右键菜单「下载」）──────────────────────────────────────

/** 终端选区 → 远端名字：取首个非空行，去掉首尾空白与 ls -F 指示符（/ @ = * | >） */
export function cleanSelectedName(sel: string): string | null {
  const line = sel.split(/\r?\n/).map((l) => l.trim()).find(Boolean)
  if (!line) return null
  const name = line.replace(/[/@=*|>]+$/, '').trim()
  return name || null
}

/** 终端下载浮层状态（onState(null) = 收起浮层，如用户取消保存对话框） */
export interface TermDownloadState {
  /** 选区名（展示） */
  name: string
  isDir: boolean
  transferred: number
  total: number
  speedBps?: number
  /** 目录任务：当前文件 / 扫描阶段说明 */
  current?: string
  fileDone?: number
  fileTotal?: number
  /** 传输句柄（取消用） */
  opId?: string
  status: 'resolving' | 'downloading' | 'done' | 'error'
  message?: string
}

/** 父目录 ls 中按名找条目（确定文件/文件夹与规范名）；找不到 → null */
async function statByName(
  sid: string,
  parentDir: string,
  name: string,
): Promise<{ entry: SftpEntry; parent: string } | null> {
  const r = await sftpRequest(sid, { op: 'ls', path: parentDir })
  if (!r.ok || !r.entries || !r.resolvedPath) return null
  const entry = r.entries.find((e) => e.name === name)
  return entry ? { entry, parent: r.resolvedPath } : null
}

/** 取消终端下载的原生传输任务（保留 .part 断点，重下自动续传） */
export function cancelTermDownload(sid: string, opId: string): void {
  void sftpRequest(sid, { op: 'transfer-cancel', opId })
}

// ── 终端选区删除（右键菜单「删除所选」）──────────────────────────────────

/** 选区解析出的远端删除目标（isDir 由父目录 ls 自动识别） */
export interface TermDeleteTarget {
  name: string
  isDir: boolean
  remotePath: string
}

/** 终端选区 → 远端删除目标：cwd/父目录 ls 确认存在并自动识别文件/文件夹 */
export async function resolveSelectionTarget(
  sid: string,
  rawName: string,
): Promise<TermDeleteTarget | { error: string }> {
  const picked = rawName.replace(/\/+$/, '')
  const abs = picked.startsWith('/')
  const cwd = await resolveRemoteCwd(sid)
  if ('error' in cwd) return { error: cwd.error }
  const parentDir = abs ? picked.slice(0, picked.lastIndexOf('/')) || '/' : cwd.dir
  const leaf = picked.slice(picked.lastIndexOf('/') + 1)
  const hit = await statByName(sid, parentDir, leaf)
  if (!hit) return { error: `在 ${parentDir} 中未找到「${leaf}」（选区需为完整文件/文件夹名）` }
  return { name: hit.entry.name, isDir: hit.entry.type === 'dir', remotePath: join(hit.parent, hit.entry.name) }
}

/** 删除远端文件/文件夹（目录递归）；递归删除大目录合法耗时长，超时放宽到 10 分钟 */
export async function deleteRemoteTarget(
  sid: string,
  target: TermDeleteTarget,
): Promise<{ ok: boolean; message?: string }> {
  const r = await sftpRequest(sid, { op: 'rm', path: target.remotePath, recursive: target.isDir }, 10 * 60 * 1000)
  return r.ok ? { ok: true } : { ok: false, message: r.message ?? '删除失败' }
}

/**
 * 下载终端选区指定的远端文件/文件夹：
 *  解析选区（相对当前目录 / 绝对路径）→ 父目录 ls 确认类型 →
 *  Tauri 壳内走原生传输（get-file/get-dir：Agent 直写本地磁盘、断点续传、
 *  保存对话框）；浏览器宿主回退 WS 流式 Blob（仅文件）。
 */
export async function downloadSelectionFromSession(
  sid: string,
  rawName: string,
  onState: (st: TermDownloadState | null) => void,
): Promise<TermDownloadState | null> {
  const picked = rawName.replace(/\/+$/, '')
  const abs = picked.startsWith('/')
  const cwd = await resolveRemoteCwd(sid)
  if ('error' in cwd) {
    const st: TermDownloadState = {
      name: picked, isDir: false, transferred: 0, total: 0, status: 'error', message: cwd.error,
    }
    onState(st)
    return st
  }
  // 相对名：cwd 即父目录；绝对路径：取 dirname
  const parentDir = abs ? picked.slice(0, picked.lastIndexOf('/')) || '/' : cwd.dir
  const leaf = picked.slice(picked.lastIndexOf('/') + 1)
  let st: TermDownloadState = { name: picked, isDir: false, transferred: 0, total: 0, status: 'resolving' }
  onState(st)

  const hit = await statByName(sid, parentDir, leaf)
  if (!hit) {
    st = { ...st, status: 'error', message: `在 ${parentDir} 中未找到「${leaf}」（选区需为完整文件/文件夹名）` }
    onState(st)
    return st
  }
  const { entry, parent } = hit
  const isDir = entry.type === 'dir'
  const remotePath = join(parent, entry.name)
  st = { ...st, isDir }
  onState(st)

  // ── Tauri：保存对话框 + 原生传输（断点续传；文件夹递归并发下载）────────
  if (isTauri) {
    const { save, open } = await import('@tauri-apps/plugin-dialog')
    let local: string | null = null
    if (isDir) {
      const dir = await open({ directory: true, multiple: false })
      if (typeof dir === 'string') {
        const sep = dir.match('\\') ? '\\' : '/'
        local = dir.replace(/[\\/]+$/, '') + sep + entry.name
      }
    } else {
      local = await save({ defaultPath: entry.name })
    }
    if (!local) {
      onState(null) // 用户取消对话框：直接收起浮层
      return null
    }
    // 统一预检（重名/空间/本地盘格式）+ 确认（覆盖 / 重命名 / 取消）
    const decisions = await preflightBatch(sid, 'download', [
      { path: remotePath, name: entry.name, sizeB: entry.sizeB, target: local },
    ])
    const decision = decisions?.[0]
    if (!decision) {
      onState(null)
      return null
    }
    if (decision.skip) {
      onState(null)
      return null
    }
    st = { ...st, status: 'downloading' }
    onState(st)
    const r = await runNativeTransfer(
      sid,
      isDir
        ? { op: 'get-dir', path: remotePath, to: decision.target, fresh: decision.fresh }
        : { op: 'get-file', path: remotePath, to: decision.target, fresh: decision.fresh },
      (p) => {
        st = {
          ...st,
          transferred: p.transferred,
          total: p.total,
          speedBps: p.speedBps,
          current: p.name,
          fileDone: p.fileDone,
          fileTotal: p.fileTotal,
          opId: p.opId,
        }
        onState(st)
      },
    )
    if (r.ok) {
      st = { ...st, transferred: r.sizeB ?? st.total, status: 'done' }
    } else {
      showTransferError(`下载 ${entry.name} 失败`, r.errorKind, r.message)
      st = { ...st, status: 'error', message: r.message }
    }
    onState(st)
    return st
  }

  // ── 浏览器宿主回退：WS 流式 Blob（仅文件）────────────────────────────
  if (isDir) {
    st = { ...st, status: 'error', message: '浏览器模式暂不支持下载文件夹，请在桌面应用中使用' }
    onState(st)
    return st
  }
  const open = await sftpRequest(sid, { op: 'download-open', path: remotePath })
  if (!open.ok || !open.opId) {
    st = { ...st, status: 'error', message: open.message ?? '打开下载失败' }
    onState(st)
    return st
  }
  const key = open.opId
  st = { ...st, status: 'downloading', total: open.sizeB ?? entry.sizeB, opId: key }
  onState(st)
  const chunks: Uint8Array[] = []
  let received = 0
  const offChunk = agentSocket.registerDownloadChunk(key, (chunk) => {
    chunks.push(chunk)
    received += chunk.byteLength
    st = { ...st, transferred: received }
    onState(st)
  })
  // 打开/读取失败（无读权限、连接断开）由 download-done(ok=false) 即刻回报，
  // 不再静默等满 20 分钟兜底超时
  let done: { ok: boolean; message?: string; errorKind?: string } = { ok: false, message: '下载超时（20 分钟无进展）' }
  await new Promise<void>((resolve) => {
    const offCtl = agentSocket.onControl((frame) => {
      if (frame.t === 'sftp-result' && frame.s === sid && frame.m.op === 'download-done' && frame.m.opId === key) {
        done = { ok: frame.m.ok, message: frame.m.message, errorKind: frame.m.errorKind }
        clearTimeout(timer)
        offCtl()
        resolve()
      }
    })
    // 大文件兜底超时（20 分钟）
    const timer = window.setTimeout(() => {
      offCtl()
      resolve()
    }, 20 * 60 * 1000)
  })
  offChunk()
  agentSocket.sendControl({ t: 'sftp', s: sid, m: { op: 'download-close', opId: key, rid: crypto.randomUUID() } })
  if (!done.ok) {
    showTransferError(`下载 ${entry.name} 失败`, done.errorKind, done.message)
    st = { ...st, status: 'error', message: done.message ?? '下载失败' }
    onState(st)
    return st
  }
  const blob = new Blob(chunks as unknown as BlobPart[])
  const a = document.createElement('a')
  a.href = URL.createObjectURL(blob)
  a.download = entry.name
  a.click()
  URL.revokeObjectURL(a.href)
  st = { ...st, transferred: blob.size, status: 'done' }
  onState(st)
  return st
}
