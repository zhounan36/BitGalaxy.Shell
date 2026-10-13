/**
 * 统一传输服务：SSH 文件管理（SftpPane）、终端拖拽上传/选区下载（termUpload）、
 * 桌宠拖拽上传（DropUploadModal）共用的传输入口与交互。
 *
 * 流程（preflightBatch）：
 *   preflight（Agent 端实测重名/权限/剩余空间/盘格式）→
 *     blockers → Modal.error 汇总（对应项跳过）
 *     重名    → 三选弹窗：覆盖 / 重命名（默认建议名，可编辑）/ 取消
 *     ≥1GB    → 大文件确认（大小、目标剩余空间、文件系统格式、告警附注）
 *   决策（target/fresh）随 get-／put- 系列操作发给 Agent：fresh = 覆盖语义
 *   （下载忽略 .part 断点、上传跳过远端 APPEND）。
 *
 * 传输失败按 errorKind 映射中文文案与建议动作（describeTransferError）。
 * AI 通道（桌宠对话）不弹窗：toolcore 复用同一 Agent 端预检并以
 * needConfirm 结构化返回，由 AI 在对话内说明并等用户指示。
 */
import { Button, Input, Modal, Typography, message } from 'antd'
import type { PreflightReport, SftpProgress, SftpRequest, SftpResult } from '@bgshell/protocol'
import { agentSocket } from './ws'
import { formatBytes } from '../utils/format'

/** 大文件提醒阈值：≥1GB（与 Agent 端 build_preflight_report 一致） */
export const LARGE_FILE_BYTES = 1024 * 1024 * 1024

// ── SFTP 请求（rid 关联 + 超时兜底）──────────────────────────────────────

export async function sftpRequest(
  sid: string,
  op: SftpRequest,
  timeoutMs = 20000,
): Promise<SftpResult> {
  const rid = crypto.randomUUID()
  return await new Promise<SftpResult>((resolve) => {
    const timer = window.setTimeout(() => {
      off()
      resolve({ op: op.op, rid, ok: false, message: '请求超时' })
    }, timeoutMs)
    const off = agentSocket.onControl((frame) => {
      if (frame.t === 'sftp-result' && frame.s === sid && frame.m.rid === rid) {
        clearTimeout(timer)
        off()
        resolve(frame.m)
      }
    })
    agentSocket.sendControl({ t: 'sftp', s: sid, m: { ...op, rid } })
  })
}

// ── 原生传输（Agent 直传本地磁盘 + 断点续传）─────────────────────────────

type NativeTransferOp = Extract<
  SftpRequest,
  { op: 'get-file' | 'get-dir' | 'put-file' | 'put-dir' }
>

export type NativeProgress = SftpProgress

export interface NativeTransferResult {
  ok: boolean
  sizeB?: number
  message?: string
  errorKind?: string
}

/** 发起原生传输并跟踪进度/结束（sftp-progress + transfer-done 帧）。
 *  无整体超时——大文件由用户取消兜底。 */
export async function runNativeTransfer(
  sid: string,
  op: NativeTransferOp,
  onProgress: (p: NativeProgress) => void,
): Promise<NativeTransferResult> {
  const open = await sftpRequest(sid, op)
  if (!open.ok || !open.opId) {
    return { ok: false, message: open.message ?? '启动传输失败', errorKind: open.errorKind }
  }
  const opId = open.opId
  return await new Promise((resolve) => {
    const off = agentSocket.onControl((frame) => {
      if (!('s' in frame) || frame.s !== sid) return
      if (frame.t === 'sftp-progress' && frame.m.opId === opId) {
        onProgress(frame.m)
      }
      if (frame.t === 'sftp-result' && frame.m.op === 'transfer-done' && frame.m.opId === opId) {
        off()
        clearTimeout(timer)
        resolve(
          frame.m.ok
            ? { ok: true, sizeB: frame.m.sizeB }
            : { ok: false, message: frame.m.message ?? '传输失败', errorKind: frame.m.errorKind },
        )
      }
    })
    // 兜底：会话断开等导致收不到结束帧
    const timer = window.setTimeout(() => {
      off()
      resolve({ ok: false, message: '传输超时（未收到完成信号）', errorKind: 'network' })
    }, 2 * 60 * 60 * 1000)
  })
}

/** 取消原生传输任务（保留断点，重新发起自动续传） */
export function cancelNativeTransfer(sid: string, opId: string): void {
  void sftpRequest(sid, { op: 'transfer-cancel', opId })
}

// ── 错误分类 → 中文文案与建议动作 ────────────────────────────────────────

export interface TransferErrorText {
  title: string
  hint?: string
}

const ERROR_KIND_TEXT: Record<string, TransferErrorText> = {
  local_io: {
    title: '本地文件访问被拒',
    hint: '本地临时文件（.part）或源文件被占用——常见于杀毒软件实时扫描、OneDrive 等同步盘锁定，或文件正被其他程序打开；请更换保存目录、稍后重试，或将应用加入杀毒软件白名单',
  },
  permission_denied: {
    title: '没有权限',
    hint: '目标目录不可写或文件归属其他用户；请检查权限、换目标目录，或用更高权限账号重试',
  },
  no_space: { title: '磁盘空间不足', hint: '请清理目标盘空间或更换存储位置后重试' },
  quota: { title: '超出磁盘配额', hint: '账户配额已满；请清理远端文件或联系管理员' },
  file_exists: { title: '目标已存在', hint: '目标位置已有同名对象，请重命名或确认覆盖' },
  text_file_busy: {
    title: '文件正被占用',
    hint: '文件正被其他进程使用（如脚本运行中）；请先停止相关进程再传输',
  },
  not_found: { title: '文件不存在', hint: '源路径不存在或已被移动/删除' },
  network: {
    title: '连接异常',
    hint: '网络中断或超时；支持断点续传，连接恢复后重新发起即可从断点继续',
  },
}

/** 把 Agent 返回的 (errorKind, message) 映射为用户可读文案与建议动作 */
export function describeTransferError(
  kind: string | undefined,
  message?: string,
): TransferErrorText {
  const t = kind ? ERROR_KIND_TEXT[kind] : undefined
  if (!t) return { title: message ?? '传输失败' }
  return { title: t.title, hint: message ? `${message}\n建议：${t.hint}` : t.hint }
}

/** 传输失败提示（title + 建议动作，一键收起） */
export function showTransferError(prefix: string, kind: string | undefined, detail?: string): void {
  const t = describeTransferError(kind, detail)
  message.error({
    content: (
      <div>
        <div>
          {prefix}：{t.title}
        </div>
        {t.hint && (
          <Typography.Text type="secondary" style={{ fontSize: 12, whiteSpace: 'pre-wrap' }}>
            {t.hint}
          </Typography.Text>
        )}
      </div>
    ),
    duration: 6,
  })
}

// ── 预检 + 确认交互 ───────────────────────────────────────────────────────

export interface PreflightItem {
  /** preflight 源路径（完整路径）：上传 = 本地源（Tauri 原生路径；浏览器 File 留空，
   *  Agent 以 sizeB 提示预检）；下载 = 远端源完整路径 */
  path?: string
  /** 浏览器 File 对象（上传方向；无本地路径，Agent 以 sizeB 提示预检） */
  file?: File
  /** 展示名 */
  name: string
  sizeB: number
  /** 完整目标路径（上传 = 远端；下载 = 本地保存路径） */
  target: string
}

export interface TransferDecision {
  /** 最终目标路径（重命名后与原 target 不同） */
  target: string
  /** 覆盖语义：下载忽略 .part 断点、上传跳过远端 APPEND，全新写入 */
  fresh: boolean
  /** 跳过（被阻断或用户放弃） */
  skip: boolean
}

interface CheckedItem {
  item: PreflightItem
  report?: PreflightReport
  error?: string
  decision?: TransferDecision
}

/** 预检单项大小：目录以 Agent 扫描结果为准（File/stat 的 sizeB 对目录无意义） */
function checkedSize(c: CheckedItem): number {
  return Math.max(c.item.sizeB, c.report?.sizeB ?? 0)
}

/** 阻断汇总弹窗（对应项将被跳过，其余继续） */
function showBlockers(blocked: CheckedItem[]): void {
  Modal.error({
    title: `${blocked.length} 个项目无法传输`,
    width: 540,
    content: (
      <ul style={{ paddingLeft: 18, margin: 0, maxHeight: 260, overflow: 'auto' }}>
        {blocked.map((b) => (
          <li key={b.item.target} style={{ marginBottom: 4 }}>
            <Typography.Text strong>{b.item.name}</Typography.Text>
            ：{b.error ?? b.report?.blockers.map((x) => x.message).join('；')}
          </li>
        ))}
      </ul>
    ),
    okText: '知道了',
  })
}

/** 重命名弹窗：默认值为建议名（上传 = 后端 suggestedPath；下载 = 本地路径加 (1)） */
function promptRename(
  name: string,
  target: string,
  suggested: string | undefined,
  isDownload: boolean,
): Promise<string | null> {
  const fallback = isDownload ? localRenamed(target) : (suggested ?? target)
  return new Promise((resolve) => {
    let value = fallback
    Modal.confirm({
      title: `重命名「${name}」`,
      width: 520,
      content: (
        <div>
          <Input defaultValue={fallback} onChange={(e) => (value = e.target.value)} />
          <Typography.Text
            type="secondary"
            style={{ fontSize: 12, display: 'block', marginTop: 8 }}
          >
            {isDownload ? '本地保存路径' : '远端目标路径'}（完整路径）
          </Typography.Text>
        </div>
      ),
      okText: '确定',
      cancelText: '取消',
      onOk: () => resolve(value.trim() || null),
      onCancel: () => resolve(null),
    })
  })
}

/** 本地路径重名建议：name → name(1).ext（下载方向后端建议名是远端格式，不适用） */
function localRenamed(target: string): string {
  const i = Math.max(target.lastIndexOf('\\'), target.lastIndexOf('/'))
  const sep = target.includes('\\') ? '\\' : '/'
  const dir = i > 0 ? target.slice(0, i) : ''
  const name = target.slice(i + 1)
  const dot = name.lastIndexOf('.')
  const stem = dot > 0 ? name.slice(0, dot) : name
  const ext = dot > 0 ? name.slice(dot) : ''
  return `${dir ? dir + sep : ''}${stem}(1)${ext}`
}

type NameChoice = { kind: 'overwrite' } | { kind: 'rename'; target: string } | null

/** 重名三选：覆盖 / 重命名（推荐）/ 取消（中止本次全部传输） */
function confirmNameConflict(
  name: string,
  target: string,
  suggested: string | undefined,
  isDownload: boolean,
): Promise<NameChoice> {
  return new Promise((resolve) => {
    const modal = Modal.confirm({
      title: `「${name}」在目标位置已存在`,
      width: 540,
      content: (
        <div>
          <div style={{ marginBottom: 8 }}>目标：{target}</div>
          <Typography.Text type="secondary">
            覆盖将替换已有文件；重命名保留原文件（建议 {isDownload ? localRenamed(target) : suggested ?? '自动生成'}
            ）；取消中止本次全部传输。
          </Typography.Text>
        </div>
      ),
      okButtonProps: { style: { display: 'none' } },
      cancelButtonProps: { style: { display: 'none' } },
      footer: (
        <>
          <Button onClick={() => { modal.destroy(); resolve(null) }}>取消</Button>
          <Button danger onClick={() => { modal.destroy(); resolve({ kind: 'overwrite' }) }}>
            覆盖
          </Button>
          <Button
            type="primary"
            onClick={() => {
              modal.destroy()
              void promptRename(name, target, suggested, isDownload).then((renamed) =>
                resolve(renamed ? { kind: 'rename', target: renamed } : null),
              )
            }}
          >
            重命名
          </Button>
        </>
      ),
    })
  })
}

/** 大文件确认：列出各项大小、目标剩余空间、盘格式；warnings 一并附注 */
function confirmLargeFiles(items: CheckedItem[], direction: 'upload' | 'download'): Promise<boolean> {
  const warnTexts = [
    ...new Set(
      items.flatMap((c) =>
        (c.report?.warnings ?? [])
          .filter((w) => w.code !== 'large_file')
          .map((w) => w.message),
      ),
    ),
  ]
  return new Promise((resolve) => {
    Modal.confirm({
      title: '大文件传输确认',
      width: 560,
      content: (
        <div>
          <ul style={{ paddingLeft: 18, margin: '0 0 8px' }}>
            {items.map((c) => {
              const free =
                direction === 'upload' ? c.report?.remoteFreeB : c.report?.localFreeB
              const fs = direction === 'upload' ? undefined : c.report?.localFs
              return (
                <li key={c.item.target} style={{ marginBottom: 2 }}>
                  <Typography.Text strong>{c.item.name}</Typography.Text>
                  ：约 {formatBytes(checkedSize(c))}
                  {c.report?.fileCount !== undefined && c.report.fileCount > 1 && `（${c.report.fileCount} 个文件）`}
                  {free !== undefined && `，目标剩余 ${formatBytes(free)}`}
                  {fs && `，${fs}`}
                </li>
              )
            })}
          </ul>
          {warnTexts.length > 0 && (
            <div style={{ marginBottom: 8 }}>
              {warnTexts.map((w) => (
                <div key={w} style={{ color: '#d46b08', fontSize: 12 }}>
                  注意：{w}
                </div>
              ))}
            </div>
          )}
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            传输量较大、耗时可能较长；传输中断不会作废——支持断点续传，重新发起即可从断点继续。
          </Typography.Text>
        </div>
      ),
      okText: '开始传输',
      cancelText: '取消',
      onOk: () => resolve(true),
      onCancel: () => resolve(false),
    })
  })
}

/**
 * 统一预检 + 确认入口（批量）。返回与 items 等长的决策数组：
 *   null           → 用户取消（中止全部）
 *   decision.skip  → 该项被阻断或放弃（调用方应跳过）
 *   decision.fresh → 覆盖语义（get-／put- 系列操作透传给 Agent）
 */
export async function preflightBatch(
  sid: string,
  direction: 'upload' | 'download',
  items: PreflightItem[],
): Promise<TransferDecision[] | null> {
  if (items.length === 0) return []

  // 1. 逐项预检（并发；Agent 端有扫描预算与探针缓存，批量友好）
  const checked: CheckedItem[] = await Promise.all(
    items.map(async (item): Promise<CheckedItem> => {
      const r = await sftpRequest(sid, {
        op: 'preflight',
        path: item.path ?? item.name,
        to: item.target,
        direction,
        sizeB: item.sizeB,
      })
      return r.ok ? { item, report: r.preflight } : { item, error: r.message ?? '预检失败' }
    }),
  )

  // 2. 阻断项：弹窗说明并跳过（其余继续）
  const blocked = checked.filter((c) => c.error || (c.report?.blockers.length ?? 0) > 0)
  if (blocked.length > 0) showBlockers(blocked)
  if (blocked.length === checked.length) {
    return checked.map(() => ({ target: '', fresh: false, skip: true }))
  }

  // 3. 重名项：串行三选；重命名后重新预检（防新名再撞车，最多 3 轮）
  for (const c of checked) {
    if (blocked.includes(c)) continue
    for (let round = 0; round < 3; round++) {
      const report = c.report
      if (!report) break
      const exists = direction === 'upload' ? report.remoteExists : report.localExists
      if (!exists) break
      const choice = await confirmNameConflict(
        c.item.name,
        c.item.target,
        report.suggestedPath,
        direction === 'download',
      )
      if (choice === null) return null
      if (choice.kind === 'overwrite') {
        c.decision = { target: c.item.target, fresh: true, skip: false }
        break
      }
      // rename：重新预检确认新名不冲突
      const r = await sftpRequest(sid, {
        op: 'preflight',
        path: c.item.path ?? c.item.name,
        to: choice.target,
        direction,
        sizeB: c.item.sizeB,
      })
      if (!r.ok) return null
      c.item = { ...c.item, target: choice.target }
      c.report = r.preflight
      const stillExists = r.preflight
        ? direction === 'upload'
          ? r.preflight.remoteExists
          : r.preflight.localExists
        : false
      if (!stillExists) {
        c.decision = { target: choice.target, fresh: true, skip: false }
        break
      }
      if (round === 2) {
        message.warning(`「${c.item.name}」多次重命名仍冲突，已跳过`)
        c.decision = { target: c.item.target, fresh: false, skip: true }
      }
    }
  }

  // 4. 大文件确认（任一项 ≥1GB；含阻断与跳过外的所有项）
  const active = checked.filter((c) => !blocked.includes(c) && !c.decision?.skip)
  const large = active.filter((c) => checkedSize(c) >= LARGE_FILE_BYTES)
  if (large.length > 0) {
    const ok = await confirmLargeFiles(large, direction)
    if (!ok) return null
  } else {
    // 无大文件弹窗时，告警以轻提示暴露（去重）
    const warnTexts = [
      ...new Set(
        active.flatMap((c) =>
          (c.report?.warnings ?? [])
            .filter((w) => w.code !== 'large_file')
            .map((w) => w.message),
        ),
      ),
    ]
    if (warnTexts.length > 0) message.warning(warnTexts.slice(0, 3).join('；'))
  }

  // 5. 汇总决策
  return checked.map((c) => c.decision ?? { target: c.item.target, fresh: false, skip: false })
}
