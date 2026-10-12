/**
 * 桌宠 AI 对话（transport-mcp 思路的 WS 轻量版）：
 * OpenAI 兼容接口（默认 DeepSeek）工具循环在 Agent 侧执行，工具复用 connector-core：
 *   list_hosts / probe_host / list_dir（只读自动放行）
 *   exec_command（黑名单硬拒；白名单直放；其余经桌宠弹卡片人工批准，TOFU fail-closed）
 * 审计同写 audit-queue.jsonl（ai.tool 事件），对话历史按 chatId 常驻内存（上限裁剪）。
 */
import { connect as tcpConnect, type Socket } from 'node:net'
import type { ClientFrame, ServerFrame } from '@bgshell/protocol'
import { AuditQueue, HostStore, KnownHosts, SettingsStore, SshSession } from '@bgshell/connector-core'

// ── 配置（SQLite settings 优先，环境变量兜底）─────────────────────────────

export interface AiConfig {
  apiKey: string
  baseUrl: string
  model: string
}

function loadAiConfig(store: SettingsStore): AiConfig | undefined {
  let apiKey = store.get<string>('ai.apiKey')
  let baseUrl = store.get<string>('ai.baseUrl')
  let model = store.get<string>('ai.model')
  apiKey ??= process.env.DEEPSEEK_API_KEY ?? process.env.OPENAI_API_KEY
  baseUrl ??= process.env.BGSH_AI_BASE_URL
  model ??= process.env.BGSH_AI_MODEL
  if (!apiKey) return undefined
  return { apiKey, baseUrl: baseUrl ?? 'https://api.deepseek.com', model: model ?? 'deepseek-chat' }
}

// ── SSH 连接池（按 hostId 复用，空闲 60s 关闭）────────────────────────────

interface PoolEntry {
  ssh: SshSession
  idleTimer: NodeJS.Timeout
}

const IDLE_CLOSE_MS = 60_000

export class AiSshPool {
  private readonly entries = new Map<string, PoolEntry>()
  private readonly opening = new Map<string, Promise<SshSession>>()
  private closed = false

  constructor(
    private readonly hostStore: HostStore,
    private readonly knownHosts: KnownHosts,
  ) {}

  async acquire(hostId: string): Promise<SshSession> {
    if (this.closed) throw new Error('Agent 已停止')
    const existing = this.entries.get(hostId)
    if (existing) {
      this.touch(hostId)
      return existing.ssh
    }
    let opening = this.opening.get(hostId)
    if (!opening) {
      opening = this.connect(hostId).finally(() => this.opening.delete(hostId))
      this.opening.set(hostId, opening)
    }
    const ssh = await opening
    this.touch(hostId)
    return ssh
  }

  evict(hostId: string): void {
    const entry = this.entries.get(hostId)
    if (!entry) return
    clearTimeout(entry.idleTimer)
    this.entries.delete(hostId)
    entry.ssh.close('evicted')
  }

  closeAll(): void {
    this.closed = true
    for (const hostId of [...this.entries.keys()]) this.evict(hostId)
  }

  private touch(hostId: string): void {
    const entry = this.entries.get(hostId)
    if (!entry) return
    clearTimeout(entry.idleTimer)
    entry.idleTimer = setTimeout(() => this.evict(hostId), IDLE_CLOSE_MS)
    entry.idleTimer.unref?.()
  }

  private async connect(hostId: string): Promise<SshSession> {
    const host = this.hostStore.get(hostId)
    if (!host) throw new Error(`主机档案不存在: ${hostId}`)
    const ssh = new SshSession(this.knownHosts, {
      onData: () => {}, // exec 通道输出由 exec() 一次性收集
      onClose: () => this.evict(hostId),
      onError: () => {},
      // fail-closed：AI 通道不做 TOFU 首连确认，未知指纹直接拒绝（提示先在工作台人工连接）
      onHostKeyUnknown: async () => false,
    })
    const ok = await ssh.open(host, { kind: 'exec' })
    if (!ok) {
      throw new Error(`主机 ${host.name} 指纹未确认：请先在工作台人工连接一次后再交给 AI 使用`)
    }
    this.entries.set(hostId, { ssh, idleTimer: setTimeout(() => {}, 0) })
    this.touch(hostId)
    return ssh
  }
}

// ── 命令安全分级（黑名单硬拒 / 白名单直放 / 其余人工批准）─────────────────

const DENY_PATTERNS: Array<{ re: RegExp; why: string }> = [
  { re: /rm\s+(-[a-zA-Z]*f[a-zA-Z]*\s+)*-(?:[a-zA-Z]*r[a-zA-Z]*f|[a-zA-Z]*f[a-zA-Z]*r)\b|rm\s+-rf?\s+\/(?:\s|$)/i, why: '递归强制删除根级路径' },
  { re: /\bmkfs(\.\w+)?\b|\bdd\s+if=/i, why: '格式化/裸写块设备' },
  { re: /\b(shutdown|reboot|poweroff|halt|init\s+[06])\b/i, why: '关机/重启' },
  { re: /:\(\)\s*\{\s*:\|:&\s*\}\s*;:|fork\s*\(\)\s*while/, why: 'fork 炸弹' },
  { re: /chmod\s+-R\s+777\s+\//i, why: '全盘放开权限' },
  { re: />\s*\/dev\/(sd|nvme|hd)/i, why: '覆写磁盘设备' },
  { re: /\b(userdel|groupdel)\b.*\b(root)\b/i, why: '删除系统账户' },
]

const READ_ONLY_RE =
  /^\s*(?:sudo\s+)?(ls|ll|cat|head|tail|pwd|whoami|id|hostname|uname|uptime|date|echo|which|env|printenv|df|free|du|wc|stat|file|ps|top\s+-b|ip\s+(a|addr)|ifconfig|netstat|ss|lsof|grep|find|git\s+(status|log|diff)|systemctl\s+(status|list|is-active)|docker\s+(ps|images)|kubectl\s+get|cat\s+\/etc\/os-release|lsb_release)\b/

function classifyCommand(command: string): 'allow' | 'deny' | 'ask' {
  for (const { re } of DENY_PATTERNS) {
    if (re.test(command)) return 'deny'
  }
  return READ_ONLY_RE.test(command) ? 'allow' : 'ask'
}

// ── LLM 工具定义与循环 ────────────────────────────────────────────────────

interface LlmMessage {
  role: 'system' | 'user' | 'assistant' | 'tool'
  content: string | null
  tool_calls?: Array<{ id: string; type: 'function'; function: { name: string; arguments: string } }>
  tool_call_id?: string
}

const SYSTEM_PROMPT = [
  '你是 BitGalaxy Shell 的桌宠助手「鲸鱼娘」，帮用户管理 SSH 主机。',
  '风格：中文、简短、活泼但专业；汇报命令结果时给出要点总结，不要大段贴原文（超 20 行只说关键部分）。',
  '可用工具：list_hosts 查主机；probe_host 测连通；exec_command 在主机上执行命令（危险命令需要用户在宠物弹窗批准）；list_dir 列远程目录。',
  '主机 id 必须先用 list_hosts 查询；用户说"web 服务器"这类称呼时按名称/标签匹配。',
  '执行失败时如实说明原因；不要编造输出。',
].join('\n')

const TOOLS = [
  {
    type: 'function' as const,
    function: {
      name: 'list_hosts',
      description: '列出所有主机档案（脱敏，不含凭据）。返回 id、名称、地址、用户、标签与 hostKeyKnown（指纹是否已确认）。',
      parameters: { type: 'object', properties: {} },
    },
  },
  {
    type: 'function' as const,
    function: {
      name: 'probe_host',
      description: 'TCP 探测主机连通性（不做 SSH 握手），返回延迟或失败原因。',
      parameters: {
        type: 'object',
        properties: { hostId: { type: 'string', description: '主机 id（先用 list_hosts 查询）' } },
        required: ['hostId'],
      },
    },
  },
  {
    type: 'function' as const,
    function: {
      name: 'exec_command',
      description:
        '在主机上执行一次性非交互命令，返回 stdout/stderr/退出码。危险命令会先向用户弹卡片请求批准，未批准则返回拒绝。',
      parameters: {
        type: 'object',
        properties: {
          hostId: { type: 'string', description: '主机 id（先用 list_hosts 查询）' },
          command: { type: 'string', description: '要执行的命令（非交互）' },
          timeoutSec: { type: 'number', description: '超时秒数，默认 30，上限 120' },
        },
        required: ['hostId', 'command'],
      },
    },
  },
  {
    type: 'function' as const,
    function: {
      name: 'list_dir',
      description: 'SFTP 列出远程目录内容（名称/类型/大小/修改时间）。',
      parameters: {
        type: 'object',
        properties: {
          hostId: { type: 'string', description: '主机 id' },
          path: { type: 'string', description: '远程目录路径，支持相对家目录' },
        },
        required: ['hostId', 'path'],
      },
    },
  },
]

interface ChatState {
  messages: LlmMessage[]
}

const MAX_HISTORY = 40
const MAX_TOOL_ROUNDS = 8
const APPROVAL_TIMEOUT_MS = 120_000

export interface AiHandlerDeps {
  hostStore: HostStore
  knownHosts: KnownHosts
  audit: AuditQueue
  settingsStore: SettingsStore
  send: (frame: ServerFrame) => void
}

/** 每条 WS 连接一个 handler（send 按连接绑定）；对话历史与 SSH 池全局共享 */
export function createAiHandler(deps: AiHandlerDeps): { handle: (frame: ClientFrame) => void; dispose: () => void } {
  const pool = new AiSshPool(deps.hostStore, deps.knownHosts)
  const approvals = new Map<string, (approved: boolean) => void>()

  const send = deps.send
  const status = (chatId: string, stage: 'thinking' | 'tool' | 'awaiting-approval', detail?: string) =>
    send({ t: 'ai-status', m: { chatId, stage, detail } })

  /** 请求用户批准（桌宠弹卡片）；超时视为拒绝 */
  const askApproval = (chatId: string, host: string, command: string): Promise<boolean> =>
    new Promise((resolve) => {
      const reqId = crypto.randomUUID()
      const timer = setTimeout(() => {
        approvals.delete(reqId)
        resolve(false)
      }, APPROVAL_TIMEOUT_MS)
      approvals.set(reqId, (approved) => {
        clearTimeout(timer)
        approvals.delete(reqId)
        resolve(approved)
      })
      send({ t: 'ai-approval', m: { chatId, reqId, host, command } })
    })

  const runTool = async (
    chatId: string,
    name: string,
    args: Record<string, unknown>,
  ): Promise<unknown> => {
    const hostStore = deps.hostStore
    switch (name) {
      case 'list_hosts':
        return {
          ok: true,
          hosts: hostStore.list().map((h) => ({
            id: h.id,
            name: h.name,
            host: h.host,
            port: h.port,
            username: h.username,
            tags: h.tags ?? [],
            hostKeyKnown: !!deps.knownHosts.find(h.host, h.port),
          })),
        }
      case 'probe_host': {
        const host = hostStore.get(String(args.hostId ?? ''))
        if (!host) return { ok: false, message: '主机档案不存在' }
        return await new Promise<{ ok: boolean; ms?: number; message?: string }>((resolve) => {
          const start = Date.now()
          const socket: Socket = tcpConnect({ host: host.host, port: host.port })
          socket.setTimeout(2500)
          socket.on('connect', () => {
            const ms = Date.now() - start
            socket.destroy()
            resolve({ ok: true, ms })
          })
          socket.on('timeout', () => {
            socket.destroy()
            resolve({ ok: false, message: '连接超时' })
          })
          socket.on('error', (err: Error) => {
            socket.destroy()
            resolve({ ok: false, message: err.message })
          })
        })
      }
      case 'exec_command': {
        const hostId = String(args.hostId ?? '')
        const command = String(args.command ?? '')
        const host = hostStore.get(hostId)
        if (!host) return { ok: false, message: '主机档案不存在' }
        if (!command.trim()) return { ok: false, message: '命令为空' }
        const cls = classifyCommand(command)
        if (cls === 'deny') {
          void deps.audit.append('ai.tool', { tool: 'exec_command', hostId, command, decision: 'deny' })
          return { ok: false, message: '命令命中安全黑名单，已拒绝执行' }
        }
        let approved = cls === 'allow'
        if (!approved) {
          status(chatId, 'awaiting-approval', command)
          approved = await askApproval(chatId, `${host.name}（${host.username}@${host.host}:${host.port}）`, command)
        }
        void deps.audit.append('ai.tool', {
          tool: 'exec_command',
          hostId,
          command,
          decision: approved ? 'allow' : 'approval-declined',
        })
        if (!approved) return { ok: false, message: '用户未批准该命令，未执行' }
        const timeoutMs = Math.min(Math.max(Number(args.timeoutSec ?? 30) * 1000, 1000), 120_000)
        try {
          const ssh = await pool.acquire(hostId)
          const r = await ssh.exec(command, timeoutMs, 64 * 1024)
          return {
            ok: true,
            exitCode: r.code,
            stdout: r.stdout,
            stderr: r.stderr,
            truncated: r.truncated,
            timedOut: r.timedOut,
          }
        } catch (err) {
          pool.evict(hostId)
          return { ok: false, message: (err as Error).message }
        }
      }
      case 'list_dir': {
        const hostId = String(args.hostId ?? '')
        const path = String(args.path ?? '.')
        try {
          const ssh = await pool.acquire(hostId)
          const sftp = await ssh.sftp()
          const raws = await new Promise<Array<{ filename: string; longname: string; attrs: { size: number; mtime: number } }>>(
            (resolve, reject) => sftp.readdir(path, (err, list) => (err ? reject(err) : resolve(list as never))),
          )
          return {
            ok: true,
            path,
            entries: raws.map((e) => ({
              name: e.filename,
              isDir: e.longname.startsWith('d'),
              sizeB: e.attrs.size,
              mtimeMs: e.attrs.mtime * 1000,
            })),
          }
        } catch (err) {
          pool.evict(hostId)
          return { ok: false, message: (err as Error).message }
        }
      }
      default:
        return { ok: false, message: `未知工具: ${name}` }
    }
  }

  const chats = new Map<string, ChatState>()
  const busy = new Set<string>()

  const runTurn = async (chatId: string, text: string): Promise<void> => {
    const cfg = loadAiConfig(deps.settingsStore)
    if (!cfg) {
      send({
        t: 'ai-result',
        m: {
          chatId,
          ok: false,
          text: '',
          message:
            '还没有配置 AI 服务：在工作台「设置 → AI 助手」填写 API Key 后重试（也可设置环境变量 DEEPSEEK_API_KEY）',
        },
      })
      return
    }
    const state = chats.get(chatId) ?? { messages: [] }
    chats.set(chatId, state)
    state.messages.push({ role: 'user', content: text })
    status(chatId, 'thinking')

    try {
      for (let round = 0; round < MAX_TOOL_ROUNDS; round++) {
        const messages: LlmMessage[] =
          state.messages[0]?.role === 'system'
            ? state.messages
            : [{ role: 'system', content: SYSTEM_PROMPT }, ...state.messages]
        const resp = await fetch(`${cfg.baseUrl.replace(/\/$/, '')}/chat/completions`, {
          method: 'POST',
          headers: { authorization: `Bearer ${cfg.apiKey}`, 'content-type': 'application/json' },
          body: JSON.stringify({ model: cfg.model, messages, tools: TOOLS, tool_choice: 'auto' }),
        })
        if (!resp.ok) {
          const body = await resp.text().catch(() => '')
          throw new Error(`AI 服务返回 ${resp.status}${body ? `: ${body.slice(0, 300)}` : ''}`)
        }
        const data = (await resp.json()) as {
          choices?: Array<{ message?: LlmMessage }>
        }
        const msg = data.choices?.[0]?.message
        if (!msg) throw new Error('AI 服务返回为空')
        state.messages.push(msg)

        if (msg.tool_calls?.length) {
          for (const tc of msg.tool_calls) {
            let args: Record<string, unknown> = {}
            try {
              args = JSON.parse(tc.function.arguments || '{}') as Record<string, unknown>
            } catch {
              // 参数不是合法 JSON：交给工具报错
            }
            status(chatId, 'tool', `${tc.function.name} ${tc.function.arguments}`.slice(0, 120))
            const out = await runTool(chatId, tc.function.name, args)
            state.messages.push({
              role: 'tool',
              tool_call_id: tc.id,
              content: JSON.stringify(out).slice(0, 16_000),
            })
          }
          continue
        }

        // 最终文本：分片下推模拟流式
        const full = msg.content ?? ''
        for (let i = 0; i < full.length; i += 160) {
          send({ t: 'ai-delta', m: { chatId, delta: full.slice(i, i + 160) } })
        }
        send({ t: 'ai-result', m: { chatId, ok: true, text: full } })
        if (state.messages.length > MAX_HISTORY) {
          state.messages.splice(0, state.messages.length - MAX_HISTORY)
        }
        return
      }
      send({ t: 'ai-result', m: { chatId, ok: false, text: '', message: '工具循环轮次超限，任务太复杂了，试试拆小一点' } })
    } catch (err) {
      send({ t: 'ai-result', m: { chatId, ok: false, text: '', message: (err as Error).message } })
    }
  }

  return {
    handle: (frame: ClientFrame) => {
      if (frame.t === 'ai') {
        const { m } = frame
        if (m.op === 'reset') {
          chats.delete(m.chatId)
          return
        }
        if (m.op === 'chat') {
          if (busy.has(m.chatId)) {
            send({ t: 'ai-result', m: { chatId: m.chatId, ok: false, text: '', message: '上一轮还在执行中，稍等一下～' } })
            return
          }
          busy.add(m.chatId)
          void runTurn(m.chatId, m.text).finally(() => busy.delete(m.chatId))
        }
        return
      }
      if (frame.t === 'ai-approve') {
        approvals.get(frame.m.reqId)?.(frame.m.approved)
      }
    },
    dispose: () => {
      for (const resolve of approvals.values()) resolve(false)
      approvals.clear()
      pool.closeAll()
    },
  }
}
