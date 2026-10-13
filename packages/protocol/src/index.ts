/**
 * 会话帧协议（设计稿 §5.2）——控制帧（WS 文本帧 / JSON）+ 数据帧（WS 二进制帧）。
 * 纯类型与编解码工具，Node（Agent）与浏览器（工作台 UI）共用，禁止引入平台专属 API。
 *
 * M1 已实装（单机离线全流程）：
 *   terminal（open/resize/close/confirm-hostkey/hosts CRUD + stdin/stdout）
 *   sftp（目录/传输，二进制 0x10/0x11 + opId 子协议）
 *   metrics（运行状态，控制帧流）+ probe（TCP 连通性探测）
 * 后续：origination 协商（M4 云端网关）、云端同步（M2）。
 */

export type Origination = 'auto' | 'local' | 'cloud'

// ── 会话种类 ─────────────────────────────────────────────────────────────
export type SessionKind = 'shell' | 'sftp' | 'metrics'

// ── 主机档案（M1：本地 hosts.json；M2 起与云端档案同步，设计稿 §5.4）──────────

export type HostAuthKind = 'password' | 'privateKey'

export interface HostAuth {
  kind: HostAuthKind
  /** kind=password 时使用 */
  password?: string
  /** kind=privateKey 时使用：私钥文件绝对路径（M1 直连版直接读本机文件；M2 起改由密库托管） */
  privateKeyPath?: string
  /** 私钥口令（可选） */
  passphrase?: string
}

export interface HostInput {
  name: string
  host: string
  port: number
  username: string
  auth: HostAuth
  tags?: string[]
}

export interface HostRecord extends HostInput {
  id: string
  createdAt: string
  updatedAt: string
}

// ── SFTP（文件管理）─────────────────────────────────────────────────────

export type SftpEntryType = 'dir' | 'file' | 'symlink' | 'other'

export interface SftpEntry {
  name: string
  type: SftpEntryType
  sizeB: number
  mtimeMs: number
}

/** 请求体（不含 rid；rid 由客户端生成并回显在结果中做关联） */
export type SftpRequest =
  | { op: 'ls'; path: string }
  | { op: 'pwd' }
  | { op: 'mkdir'; path: string }
  | { op: 'rm'; path: string; recursive?: boolean }
  | { op: 'rename'; from: string; to: string }
  | { op: 'upload-open'; path: string }
  | { op: 'upload-close'; opId: string }
  | { op: 'download-open'; path: string }
  | { op: 'download-close'; opId: string }
  /** 传输预检：重名/权限/剩余空间/目标盘格式（上传下载共用，报告随 sftp-result 返回） */
  | { op: 'preflight'; path: string; to: string; direction: 'upload' | 'download'; sizeB?: number }
  /** 原生传输（Agent 直传本地磁盘，断点续传）：下载远端文件到本地路径 */
  | { op: 'get-file'; path: string; to: string; fresh?: boolean }
  /** 递归下载远端目录到本地目录 */
  | { op: 'get-dir'; path: string; to: string; fresh?: boolean }
  /** 上传本地文件到远端路径（远端已有部分时 APPEND 续传） */
  | { op: 'put-file'; path: string; to: string; fresh?: boolean }
  /** 递归上传本地目录到远端目录 */
  | { op: 'put-dir'; path: string; to: string; fresh?: boolean }
  /** 取消原生传输任务（保留 .part 以便续传） */
  | { op: 'transfer-cancel'; opId: string }

export type SftpOp = SftpRequest & { rid: string }

/** 失败分类（Agent 侧 classify_sftp_error 产出） */
export type SftpErrorKind =
  | 'permission_denied'
  | 'no_space'
  | 'quota'
  | 'file_exists'
  | 'text_file_busy'
  | 'not_found'
  | 'network'
  | 'unknown'

/** 预检单项 code（blockers 与 warnings 共用） */
export type PreflightCode =
  | 'target_exists'
  | 'no_space'
  | 'fs_4gb_limit'
  | 'no_permission'
  | 'large_file'
  | 'free_unknown'
  | 'scan_truncated'
  | 'not_found'

export interface PreflightItem {
  code: PreflightCode | string
  message: string
}

/** 传输预检报告（上传：path=本地源、to=远端目标；下载：path=远端源、to=本地目标） */
export interface PreflightReport {
  /** 传输总字节数（目录扫描被截断时为下限） */
  sizeB: number
  /** 文件数（目录任务；单文件 = 1） */
  fileCount: number
  /** 目录扫描超预算被截断（sizeB 为下限） */
  scanTruncated: boolean
  /** 上传：远端目标已存在（重名） */
  remoteExists: boolean
  remoteSizeB?: number
  /** 远端可用字节数（服务器不支持 statvfs 扩展时缺省） */
  remoteFreeB?: number
  /** 远端目标目录可写（探针文件实测） */
  remoteWritable: boolean
  /** 下载：本地目标已存在 */
  localExists: boolean
  /** 本地可用字节数 */
  localFreeB?: number
  /** 本地卷格式（NTFS/FAT32/exFAT…） */
  localFs?: string
  /** 本地单文件大小上限（FAT32 = 4GiB-1；缺省 = 不设限） */
  localMaxFileB?: number
  /** 重名时的不冲突建议路径（name(1).ext 递增） */
  suggestedPath?: string
  /** 警告（不阻断，但应让用户知情） */
  warnings: PreflightItem[]
  /** 阻断原因（满足任一即不应发起传输） */
  blockers: PreflightItem[]
}

export interface SftpResult {
  op: string
  ok: boolean
  /** 回显请求关联 id（download-done/transfer-done 例外，仅带 opId） */
  rid?: string
  /** 传输操作句柄（upload-open/download-open/get 系列/put 系列返回） */
  opId?: string
  resolvedPath?: string
  entries?: SftpEntry[]
  sizeB?: number
  /** pwd op 附带：远端主机名（用于 OSC 7 上报的主机校验，取不到时缺省） */
  host?: string
  message?: string
  /** 失败分类（前端据此映射中文文案与建议动作） */
  errorKind?: SftpErrorKind | string
  /** preflight op 的预检报告 */
  preflight?: PreflightReport
}

/** 原生传输进度（get/put 系列任务；旧字段 opId/transferred/total 与流式下载共用） */
export interface SftpProgress {
  opId: string
  transferred: number
  total: number
  /** 当前文件（目录任务为远端相对路径；扫描阶段为阶段说明） */
  name?: string
  /** 已完成文件数（目录任务） */
  fileDone?: number
  /** 文件总数（目录任务；缺省 = 尚在扫描或单文件任务） */
  fileTotal?: number
  /** 平滑速度（字节/秒） */
  speedBps?: number
  /** 本次续传已跳过的字节数（断点续传起点，0 = 全新传输） */
  resumedB?: number
}

// ── Metrics（运行状态采集）───────────────────────────────────────────────

export interface MetricsDisk {
  source: string
  target: string
  sizeB: number
  usedB: number
  usedPct: number
}

export interface MetricsProcess {
  pid: string
  cpuPct: number
  memPct: number
  command: string
}

export interface MetricsSample {
  hostname: string
  kernel: string
  uptimeSec: number
  load1: number
  load5: number
  load15: number
  memTotalB: number
  memUsedB: number
  disks: MetricsDisk[]
  processes: MetricsProcess[]
  ts: string
}

// ── Agent 自身控制（桌面形态：打包 exe 的启动/退出/自启管理）────────────────

export type AgentOp =
  | { op: 'get-info' }
  | { op: 'set-autostart'; enabled: boolean } // 写注册表 Run 键（Windows）
  | { op: 'set-open-on-start'; enabled: boolean } // 手动启动时是否自动打开工作台
  | { op: 'shutdown' } // 优雅退出 Agent 进程

export interface AgentResult {
  op: string
  ok: boolean
  autostart?: boolean
  openOnStart?: boolean
  version?: string
  message?: string
}

// ── AI 助手（桌宠对话：LLM 工具循环在 Agent 侧执行，工具集见 transport-ws/src/ai.ts）──

export type AiStage = 'thinking' | 'tool' | 'awaiting-approval'

/** AI 请求用户批准的命令执行（桌宠弹卡片，批准后 Agent 继续工具循环） */
export interface AiApprovalRequest {
  chatId: string
  reqId: string
  host: string
  command: string
}

/** AI 工具调用事件：running 先推（卡片 spinner + 单条终止），终态同 toolCallId 更新 */
export interface AiToolEvent {
  chatId: string
  toolCallId: string
  name: string
  detail?: string
  stage: 'running' | 'done' | 'failed' | 'cancelled'
  summary?: string
  /** 可展开的完整输出（stdout/stderr 或错误详情） */
  output?: string
}

export type AiOp =
  | { op: 'chat'; chatId: string; text: string }
  | { op: 'reset'; chatId: string }
  | { op: 'cancel'; chatId: string }
  /** 只终止 toolCallId 对应的单条命令，对话继续 */
  | { op: 'cancel-tool'; chatId: string; toolCallId: string }

// ── 设置（SQLite 键值存储，统一配置读写）──────────────────────────────────

export type SettingsOp =
  | { op: 'list' }
  | { op: 'get'; key: string }
  | { op: 'set'; key: string; value: unknown }
  | { op: 'delete'; key: string }

export interface SettingsResult {
  ok: boolean
  items?: Record<string, unknown>
  value?: unknown
  message?: string
}

// ── 控制帧：客户端 → Agent（WS 文本帧，JSON）──────────────────────────────

export type HostOp =
  | { op: 'list' }
  | { op: 'create'; host: HostInput }
  | { op: 'update'; host: HostInput & { id: string } }
  | { op: 'delete'; id: string }

export interface OpenOptions {
  hostId: string
  kind?: SessionKind
  cols?: number
  rows?: number
  term?: string
  /** 发起位置（M4 云端网关生效；直连/NM 恒为 local） */
  origination?: Origination
}

export type ClientFrame =
  | { t: 'open'; s: string; m: OpenOptions }
  | { t: 'resize'; s: string; m: { cols: number; rows: number } }
  | { t: 'close'; s: string; m: { reason?: string } }
  | { t: 'confirm-hostkey'; s: string; m: { accepted: boolean } }
  | { t: 'retry-auth'; s: string; m: { password?: string } } // password 缺省 = 取消重试
  | { t: 'hosts'; m: HostOp }
  | { t: 'sftp'; s: string; m: SftpOp }
  | { t: 'metrics'; s: string; m: { intervalSec: number } } // 0 = 暂停采集
  | { t: 'probe'; m: { hostId: string } }
  | { t: 'agent'; m: AgentOp }
  | { t: 'ai'; m: AiOp }
  | { t: 'ai-approve'; m: { chatId: string; reqId: string; approved: boolean } }
  | { t: 'settings'; m: SettingsOp }

// ── 控制帧：Agent → 客户端 ───────────────────────────────────────────────

export interface HostKeyInfo {
  host: string
  port: number
  keyType: string
  fingerprint: string
}

/** 密码认证被拒：Agent 请求工作台弹窗重输密码（auth-retry 帧） */
export interface AuthRetryInfo {
  username: string
  host: string
  port: number
  message: string
}

export type ServerFrame =
  | { t: 'session-opened'; s: string; m: { hostId: string; kind: SessionKind } }
  | { t: 'session-closed'; s: string; m: { reason: string } }
  | { t: 'hostkey-unknown'; s: string; m: HostKeyInfo }
  | { t: 'auth-retry'; s: string; m: AuthRetryInfo }
  | { t: 'hosts'; m: { op: string; ok: boolean; hosts?: HostRecord[]; host?: HostRecord; message?: string } }
  | { t: 'sftp-result'; s: string; m: SftpResult }
  | { t: 'sftp-progress'; s: string; m: SftpProgress }
  | { t: 'metrics-data'; s: string; m: MetricsSample }
  | { t: 'probe-result'; m: { hostId: string; ok: boolean; ms?: number; message?: string } }
  | { t: 'agent-result'; m: AgentResult }
  | { t: 'ai-status'; m: { chatId: string; stage: AiStage; detail?: string } }
  | { t: 'ai-thinking'; m: { chatId: string; delta: string } }
  | { t: 'ai-delta'; m: { chatId: string; delta: string } }
  | { t: 'ai-result'; m: { chatId: string; ok: boolean; text: string; message?: string } }
  | { t: 'ai-approval'; m: AiApprovalRequest }
  | { t: 'ai-tool'; m: AiToolEvent }
  | { t: 'settings-result'; m: SettingsResult }
  | { t: 'error'; s?: string; m: { code: string; message: string } }

// ── 数据帧（WS 二进制帧）────────────────────────────────────────────────
// 基础布局：[type:1B][sidLen:1B][sid:UTF-8][payload]（多会话复用一条 WS，帧内嵌 sessionId）
// SFTP 传输布局：payload = [opIdLen:1B][opId:UTF-8][chunk]（对设计稿 §5.2 的实现细化）

export const DataFrameType = {
  Stdout: 0x01,
  Stdin: 0x02,
  Stderr: 0x03,
  SftpUpload: 0x10,
  SftpDownload: 0x11,
} as const

export function encodeDataFrame(type: number, sessionId: string, payload: Uint8Array): Uint8Array {
  const sid = new TextEncoder().encode(sessionId)
  const out = new Uint8Array(2 + sid.length + payload.length)
  out[0] = type
  out[1] = sid.length
  out.set(sid, 2)
  out.set(payload, 2 + sid.length)
  return out
}

export function decodeDataFrame(data: Uint8Array): { type: number; sid: string; payload: Uint8Array } {
  const type = data[0]
  const sidLen = data[1]
  const sid = new TextDecoder().decode(data.subarray(2, 2 + sidLen))
  return { type, sid, payload: data.subarray(2 + sidLen) }
}

/** SFTP 传输 payload：[opIdLen:1B][opId:UTF-8][chunk] */
export function encodeOpPayload(opId: string, chunk: Uint8Array): Uint8Array {
  const id = new TextEncoder().encode(opId)
  const out = new Uint8Array(1 + id.length + chunk.length)
  out[0] = id.length
  out.set(id, 1)
  out.set(chunk, 1 + id.length)
  return out
}

export function decodeOpPayload(data: Uint8Array): { opId: string; chunk: Uint8Array } {
  const len = data[0]
  const opId = new TextDecoder().decode(data.subarray(1, 1 + len))
  return { opId, chunk: data.subarray(1 + len) }
}
