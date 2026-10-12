import { connect as tcpConnect, type Socket } from 'node:net'
import { z } from 'zod'
import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js'
import type { SFTPWrapper } from 'ssh2'
import {
  buildMetricsCommand,
  parseMetrics,
  type AuditQueue,
  type HostStore,
  type KnownHosts,
} from '@bgshell/connector-core'
import { ToolError, ToolErrorCode } from './errors.js'
import type { McpHostPool } from './hostPool.js'
import { classifyCommand, type AiPolicy } from './policy.js'

export interface ToolContext {
  hostStore: HostStore
  knownHosts: KnownHosts
  audit: AuditQueue
  policy: AiPolicy
  pool: McpHostPool
}

/** 写入内容硬顶（防 AI 生成超大文件撑爆通道/目标盘） */
const MAX_WRITE_BYTES = 1024 * 1024

type StatResult = Parameters<Parameters<SFTPWrapper['stat']>[1]>[1]

// ── 结果与审计封装 ─────────────────────────────────────────────────────────

function textResult(obj: unknown): CallToolResult {
  return { content: [{ type: 'text', text: JSON.stringify(obj) }] }
}

function errResult(code: string, message: string): CallToolResult {
  return { content: [{ type: 'text', text: JSON.stringify({ ok: false, code, message }) }], isError: true }
}

/** 拒绝类错误 → 决策字段（执行失败类错误决策仍为 allow，见 audit 包装） */
const DECISION_BY_CODE: Record<string, string> = {
  [ToolErrorCode.PolicyDenied]: 'policy-denied',
  [ToolErrorCode.ElicitDeclined]: 'elicit-declined',
  [ToolErrorCode.ElicitUnsupported]: 'elicit-unsupported',
  [ToolErrorCode.Disabled]: 'tool-disabled',
}

/** handler 通过 detail 回填审计上下文（command/exitCode 等）；args 已由 SDK 按 zod schema 校验 */
type ToolRun = (args: AnyArgs, detail: Record<string, unknown>) => Promise<Record<string, unknown>>
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type AnyArgs = any

/**
 * 工具处理器包装：统一结果序列化、错误映射、ai.tool 审计（含被拒绝的调用）。
 */
function audited(
  ctx: ToolContext,
  server: McpServer,
  tool: string,
  hostIdOf: (args: { hostId?: string }) => string | undefined,
  run: ToolRun,
) {
  return async (args: { hostId?: string }): Promise<CallToolResult> => {
    const started = Date.now()
    const detail: Record<string, unknown> = {}
    let error: ToolError | undefined
    let payload: Record<string, unknown> | undefined
    try {
      payload = await run(args, detail)
    } catch (err) {
      error =
        err instanceof ToolError
          ? err
          : new ToolError('internal-error', (err as Error).message)
    }
    const client = server.server.getClientVersion()
    // 先落审计再回结果：保证客户端收到结果时该次调用必然已留痕
    await ctx.audit.append('ai.tool', {
      tool,
      hostId: hostIdOf(args),
      client: client ? `${client.name}/${client.version}` : 'unknown',
      decision: error ? (DECISION_BY_CODE[error.code] ?? 'allow') : 'allow',
      error: error?.code,
      ms: Date.now() - started,
      ...detail,
    })
    if (error) return errResult(error.code, error.message)
    return textResult({ ok: true, ...payload })
  }
}

// ── 人工审批（MCP elicitation，docs/ai-design.md §4.3）────────────────────

async function elicitApproval(
  server: McpServer,
  message: string,
): Promise<'accepted' | 'declined' | 'unsupported'> {
  const low = server.server
  if (!low.getClientCapabilities()?.elicitation) return 'unsupported'
  try {
    const res = await low.elicitInput(
      {
        mode: 'form',
        message,
        requestedSchema: {
          type: 'object' as const,
          properties: {
            approve: {
              type: 'boolean' as const,
              title: '允许执行',
              description: '勾选并提交以允许 AI 执行该操作',
            },
          },
          required: ['approve'],
        },
      },
      { timeout: 120_000 },
    )
    const approve = (res.content as { approve?: unknown } | undefined)?.approve
    return res.action === 'accept' && approve === true ? 'accepted' : 'declined'
  } catch {
    return 'declined' // 超时/协议错误一律视为拒绝（fail-closed）
  }
}

// ── 小工具函数 ────────────────────────────────────────────────────────────

function hostLabel(ctx: ToolContext, hostId: string): string {
  const h = ctx.hostStore.get(hostId)
  return h ? `${h.name}（${h.username}@${h.host}:${h.port}）` : hostId
}

function clamp(v: number, min: number, max: number): number {
  return Math.min(Math.max(v, min), max)
}

function probeTcp(host: string, port: number): Promise<{ ok: boolean; ms?: number; message?: string }> {
  return new Promise((resolve) => {
    const start = Date.now()
    const socket: Socket = tcpConnect({ host, port })
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

function sftpFail(path: string, err: unknown): ToolError {
  return new ToolError(ToolErrorCode.SftpFailed, `路径 ${path}: ${(err as Error).message}`)
}

async function sftpReadText(
  sftp: SFTPWrapper,
  path: string,
  offset: number,
  maxBytes: number,
): Promise<{ content: string; sizeB: number; truncated: boolean }> {
  try {
    const stat: StatResult = await new Promise((resolve, reject) =>
      sftp.stat(path, (err, s) => (err ? reject(err) : resolve(s))),
    )
    const sizeB = stat.size
    if (sizeB === 0) return { content: '', sizeB: 0, truncated: false }
    const end = Math.min(offset + maxBytes, sizeB) - 1
    const chunks: Buffer[] = []
    await new Promise<void>((resolve, reject) => {
      const stream = sftp.createReadStream(path, { start: offset, end })
      stream.on('data', (c: Buffer) => chunks.push(c))
      stream.on('error', reject)
      stream.on('close', () => resolve())
    })
    const buf = Buffer.concat(chunks)
    return { content: buf.toString('utf8'), sizeB, truncated: offset + buf.length < sizeB }
  } catch (err) {
    throw sftpFail(path, err)
  }
}

async function sftpWriteText(
  sftp: SFTPWrapper,
  path: string,
  content: string,
  mode: 'overwrite' | 'append',
): Promise<void> {
  try {
    await new Promise<void>((resolve, reject) => {
      const stream = sftp.createWriteStream(path, { flags: mode === 'append' ? 'a' : 'w' })
      stream.on('error', reject)
      stream.on('close', () => resolve())
      stream.end(content, 'utf8')
    })
  } catch (err) {
    throw sftpFail(path, err)
  }
}

/** 写操作统一闸门：write.enabled → defaultAction（docs/ai-design.md §4.1） */
async function guardWrite(
  ctx: ToolContext,
  server: McpServer,
  hostId: string,
  describe: string,
): Promise<void> {
  if (!ctx.policy.write.enabled) {
    throw new ToolError(ToolErrorCode.Disabled, '写操作已被策略禁用（ai-policy.json write.enabled=false）')
  }
  const action = ctx.policy.write.defaultAction
  if (action === 'deny') {
    throw new ToolError(ToolErrorCode.PolicyDenied, '写操作被默认拒绝（ai-policy.json write.defaultAction=deny）')
  }
  if (action === 'ask') {
    const r = await elicitApproval(server, `允许 AI 执行写操作？\n\n${describe}`)
    if (r === 'unsupported') {
      throw new ToolError(
        ToolErrorCode.ElicitUnsupported,
        '当前 AI 客户端不支持审批交互，已拒绝。可将 write.defaultAction 改为 allow 或调整 ai-policy.json 后重试',
      )
    }
    if (r === 'declined') throw new ToolError(ToolErrorCode.ElicitDeclined, '用户拒绝了该写操作')
  }
}

// ── 工具注册（docs/ai-design.md §3）───────────────────────────────────────

const HOST_ID = { hostId: z.string().describe('主机 id（先用 list_hosts 查询）') }

export function registerTools(server: McpServer, ctx: ToolContext): void {
  const A = (tool: string, hostIdOf: (a: { hostId?: string }) => string | undefined, run: ToolRun) =>
    audited(ctx, server, tool, hostIdOf, run)

  // ── 只读（R）：自动放行 ──

  server.registerTool(
    'list_hosts',
    {
      title: '列出主机',
      description:
        '列出 BitGalaxy Shell 主机档案（脱敏：不含任何凭据）。返回 id、名称、地址、用户、标签与 hostKeyKnown（指纹是否已确认；false 的主机需先在工作台人工连接一次）。',
      inputSchema: {},
      annotations: { readOnlyHint: true },
    },
    A('list_hosts', () => undefined, async () => ({
      hosts: ctx.hostStore.list().map((h) => ({
        id: h.id,
        name: h.name,
        host: h.host,
        port: h.port,
        username: h.username,
        tags: h.tags ?? [],
        hostKeyKnown: !!ctx.knownHosts.find(h.host, h.port),
      })),
    })),
  )

  server.registerTool(
    'probe_host',
    {
      title: '探测主机连通性',
      description: '对目标主机做 TCP 端口探测（不做 SSH 握手，不使用凭据），返回延迟或失败原因。',
      inputSchema: HOST_ID,
      annotations: { readOnlyHint: true },
    },
    A('probe_host', (a) => a.hostId, async (a) => {
      const host = ctx.hostStore.get(a.hostId)
      if (!host) throw new ToolError(ToolErrorCode.HostNotFound, `主机档案不存在: ${a.hostId}`)
      return probeTcp(host.host, host.port)
    }),
  )

  server.registerTool(
    'get_metrics',
    {
      title: '采集主机运行指标',
      description:
        '采集一次主机运行指标（uptime、负载、内存、磁盘、top 进程）。适用于 Linux 目标；主机需已在已知指纹列表。',
      inputSchema: HOST_ID,
      annotations: { readOnlyHint: true },
    },
    A('get_metrics', (a) => a.hostId, async (a) => {
      const ssh = await ctx.pool.acquire(a.hostId)
      const r = await ssh.exec(
        buildMetricsCommand(),
        Math.min(ctx.policy.limits.execTimeoutMs, 15_000),
        ctx.policy.limits.maxExecOutputBytes,
      )
      if (r.timedOut) throw new ToolError(ToolErrorCode.ExecTimeout, '指标采集超时')
      return { sample: parseMetrics(r.stdout) }
    }),
  )

  server.registerTool(
    'list_dir',
    {
      title: '列远程目录',
      description: 'SFTP 列出远程目录内容（名称/类型/大小/mtime）。path 支持相对路径（相对登录用户家目录）。',
      inputSchema: { ...HOST_ID, path: z.string().describe('远程目录路径') },
      annotations: { readOnlyHint: true },
    },
    A('list_dir', (a) => a.hostId, async (a) => {
      const ctrl = await ctx.pool.acquireSftp(a.hostId)
      const r = await ctrl.ls(a.path)
      return { resolvedPath: r.resolvedPath, entries: r.entries }
    }),
  )

  server.registerTool(
    'read_file',
    {
      title: '读远程文本文件',
      description:
        'SFTP 读取远程文本文件（UTF-8）。默认最多读取 256KB（受策略钳制），超限返回截断内容并标注 truncated 与 sizeB。',
      inputSchema: {
        ...HOST_ID,
        path: z.string().describe('远程文件路径'),
        offset: z.number().int().min(0).optional().describe('起始字节偏移（默认 0）'),
        maxBytes: z.number().int().min(1).optional().describe('本次最多读取字节数（受策略上限钳制）'),
      },
      annotations: { readOnlyHint: true },
    },
    A('read_file', (a) => a.hostId, async (a) => {
      const maxBytes = clamp(a.maxBytes ?? ctx.policy.limits.maxReadFileBytes, 1, ctx.policy.limits.maxReadFileBytes)
      const offset = a.offset ?? 0
      const sftp = await ctx.pool.acquireSftpRaw(a.hostId)
      const r = await sftpReadText(sftp, a.path, offset, maxBytes)
      return { path: a.path, offset, ...r }
    }),
  )

  // ── 命令执行（X）：命令分类器 + 审批 ──

  server.registerTool(
    'exec_command',
    {
      title: '执行远程命令',
      description:
        '在目标主机上执行一次性非交互命令（ssh exec 通道），返回 stdout/stderr/退出码。命令经安全策略分级：内置黑名单直接拒绝；白名单直接放行；其余按 ai-policy.json defaultAction（默认需用户在客户端审批）。超时默认 30s（可传 timeoutMs，受策略上限钳制），输出超 64KB 截断。',
      inputSchema: {
        ...HOST_ID,
        command: z.string().describe('要执行的命令（非交互；交互式命令会因 stdin 关闭而失败）'),
        timeoutMs: z.number().int().min(1000).optional().describe('超时毫秒数（受策略上限钳制）'),
      },
    },
    A('exec_command', (a) => a.hostId, async (a, detail) => {
      if (!ctx.policy.exec.enabled) {
        throw new ToolError(ToolErrorCode.Disabled, '命令执行已被策略禁用（ai-policy.json exec.enabled=false）')
      }
      detail.command = a.command
      const cls = classifyCommand(a.command, ctx.policy)
      if (cls.action === 'deny') {
        throw new ToolError(
          ToolErrorCode.PolicyDenied,
          `命令被安全策略拒绝${cls.pattern ? `（命中${cls.reason === 'deny-pattern' ? '黑名单' : '默认拒绝'}规则: ${cls.pattern}）` : ''}`,
        )
      }
      if (cls.action === 'ask') {
        const r = await elicitApproval(
          server,
          `允许 AI 在主机 ${hostLabel(ctx, a.hostId)} 上执行命令？\n\n${a.command}`,
        )
        if (r === 'unsupported') {
          throw new ToolError(
            ToolErrorCode.ElicitUnsupported,
            '当前 AI 客户端不支持审批交互，已拒绝。若该命令可信，可将其加入 ai-policy.json 白名单后重试',
          )
        }
        if (r === 'declined') throw new ToolError(ToolErrorCode.ElicitDeclined, '用户拒绝了该命令执行')
      }
      const timeoutMs = clamp(a.timeoutMs ?? ctx.policy.limits.execTimeoutMs, 1000, ctx.policy.limits.execTimeoutMs)
      try {
        const ssh = await ctx.pool.acquire(a.hostId)
        const r = await ssh.exec(a.command, timeoutMs, ctx.policy.limits.maxExecOutputBytes)
        detail.exitCode = r.code
        return {
          exitCode: r.code,
          stdout: r.stdout,
          stderr: r.stderr,
          truncated: r.truncated,
          timedOut: r.timedOut,
        }
      } catch (err) {
        if (err instanceof ToolError) throw err
        throw new ToolError(ToolErrorCode.ExecFailed, (err as Error).message)
      }
    }),
  )

  // ── 写操作（W）：策略开关 + 审批 ──

  server.registerTool(
    'write_file',
    {
      title: '写远程文本文件',
      description:
        'SFTP 写入远程文本文件（UTF-8，最大 1MB）。mode=overwrite 覆盖 / append 追加。受写策略与审批约束。',
      inputSchema: {
        ...HOST_ID,
        path: z.string().describe('远程文件路径'),
        content: z.string().describe('写入内容'),
        mode: z.enum(['overwrite', 'append']).optional().describe('默认 overwrite'),
      },
    },
    A('write_file', (a) => a.hostId, async (a, detail) => {
      const mode = a.mode ?? 'overwrite'
      detail.path = a.path
      detail.mode = mode
      if (Buffer.byteLength(a.content, 'utf8') > MAX_WRITE_BYTES) {
        throw new ToolError(ToolErrorCode.PolicyDenied, `内容超过 ${MAX_WRITE_BYTES} 字节上限，拒绝写入`)
      }
      await guardWrite(ctx, server, a.hostId, `主机 ${hostLabel(ctx, a.hostId)}\n路径: ${a.path}（${mode}，${Buffer.byteLength(a.content, 'utf8')} 字节）`)
      const sftp = await ctx.pool.acquireSftpRaw(a.hostId)
      await sftpWriteText(sftp, a.path, a.content, mode)
      return { path: a.path, mode, bytes: Buffer.byteLength(a.content, 'utf8') }
    }),
  )

  server.registerTool(
    'make_dir',
    {
      title: '建远程目录',
      description: 'SFTP 创建远程目录（单级，等价 mkdir）。受写策略与审批约束。',
      inputSchema: { ...HOST_ID, path: z.string().describe('远程目录路径') },
    },
    A('make_dir', (a) => a.hostId, async (a, detail) => {
      detail.path = a.path
      await guardWrite(ctx, server, a.hostId, `主机 ${hostLabel(ctx, a.hostId)}\n新建目录: ${a.path}`)
      const ctrl = await ctx.pool.acquireSftp(a.hostId)
      await ctrl.mkdir(a.path)
      return { path: a.path }
    }),
  )

  server.registerTool(
    'rename_path',
    {
      title: '改名/移动远程路径',
      description: 'SFTP 改名或移动远程文件/目录。受写策略与审批约束。',
      inputSchema: { ...HOST_ID, from: z.string().describe('原路径'), to: z.string().describe('目标路径') },
    },
    A('rename_path', (a) => a.hostId, async (a, detail) => {
      detail.from = a.from
      detail.to = a.to
      await guardWrite(ctx, server, a.hostId, `主机 ${hostLabel(ctx, a.hostId)}\n${a.from} → ${a.to}`)
      const ctrl = await ctx.pool.acquireSftp(a.hostId)
      await ctrl.rename(a.from, a.to)
      return { from: a.from, to: a.to }
    }),
  )

  server.registerTool(
    'delete_path',
    {
      title: '删除远程路径',
      description:
        'SFTP 删除远程文件；recursive=true 删除整个目录树（危险）。受写策略与审批约束；递归删除根目录/家目录被硬性拒绝。',
      inputSchema: {
        ...HOST_ID,
        path: z.string().describe('远程路径'),
        recursive: z.boolean().optional().describe('递归删除目录树'),
      },
      annotations: { destructiveHint: true },
    },
    A('delete_path', (a) => a.hostId, async (a, detail) => {
      const recursive = a.recursive === true
      detail.path = a.path
      detail.recursive = recursive
      if (recursive && /^(?:\/|~\/?)?$/.test(a.path.trim())) {
        throw new ToolError(ToolErrorCode.PolicyDenied, '拒绝递归删除根目录/家目录')
      }
      await guardWrite(
        ctx,
        server,
        a.hostId,
        `主机 ${hostLabel(ctx, a.hostId)}\n删除: ${a.path}${recursive ? '（递归删除整个目录树）' : ''}`,
      )
      const ctrl = await ctx.pool.acquireSftp(a.hostId)
      try {
        await ctrl.rm(a.path, recursive)
      } catch (err) {
        throw sftpFail(a.path, err)
      }
      return { path: a.path, recursive }
    }),
  )
}
