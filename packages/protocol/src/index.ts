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
  | { op: 'mkdir'; path: string }
  | { op: 'rm'; path: string; recursive?: boolean }
  | { op: 'rename'; from: string; to: string }
  | { op: 'upload-open'; path: string }
  | { op: 'upload-close'; opId: string }
  | { op: 'download-open'; path: string }
  | { op: 'download-close'; opId: string }

export type SftpOp = SftpRequest & { rid: string }

export interface SftpResult {
  op: string
  ok: boolean
  /** 回显请求关联 id（download-done 例外，仅带 opId） */
  rid?: string
  /** 传输操作句柄（upload/download-open 返回） */
  opId?: string
  resolvedPath?: string
  entries?: SftpEntry[]
  sizeB?: number
  message?: string
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
  | { t: 'hosts'; m: HostOp }
  | { t: 'sftp'; s: string; m: SftpOp }
  | { t: 'metrics'; s: string; m: { intervalSec: number } } // 0 = 暂停采集
  | { t: 'probe'; m: { hostId: string } }
  | { t: 'agent'; m: AgentOp }

// ── 控制帧：Agent → 客户端 ───────────────────────────────────────────────

export interface HostKeyInfo {
  host: string
  port: number
  keyType: string
  fingerprint: string
}

export type ServerFrame =
  | { t: 'session-opened'; s: string; m: { hostId: string; kind: SessionKind } }
  | { t: 'session-closed'; s: string; m: { reason: string } }
  | { t: 'hostkey-unknown'; s: string; m: HostKeyInfo }
  | { t: 'hosts'; m: { op: string; ok: boolean; hosts?: HostRecord[]; host?: HostRecord; message?: string } }
  | { t: 'sftp-result'; s: string; m: SftpResult }
  | { t: 'sftp-progress'; s: string; m: { opId: string; transferred: number; total: number } }
  | { t: 'metrics-data'; s: string; m: MetricsSample }
  | { t: 'probe-result'; m: { hostId: string; ok: boolean; ms?: number; message?: string } }
  | { t: 'agent-result'; m: AgentResult }
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
