import { join } from 'node:path'
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { AuditQueue, Db, HostStore, KnownHosts } from '@bgshell/connector-core'
import { McpHostPool } from './hostPool.js'
import { loadAiPolicy } from './policy.js'
import { registerTools } from './tools.js'

export { BUILTIN_DENY_PATTERNS, classifyCommand, defaultPolicy, loadAiPolicy, type AiPolicy } from './policy.js'
export { McpHostPool } from './hostPool.js'
export { registerTools, type ToolContext } from './tools.js'

export interface McpStdioOptions {
  /** 与桌面 Agent 共享的数据目录（hosts.json / known-hosts.json / ai-policy.json / 审计） */
  dataDir: string
  version?: string
}

/**
 * Agent 接入面④：MCP server（stdio，docs/ai-design.md §2）。
 * 由 AI 客户端作为子进程拉起（bgshell-agent --mcp-stdio）；
 * stdout 专用于 MCP 协议，诊断信息只走 stderr。
 */
export async function runMcpStdio(opts: McpStdioOptions): Promise<void> {
  const db = new Db(join(opts.dataDir, 'bitgalaxy.db'))
  db.migrateFromJson(opts.dataDir)
  const hostStore = new HostStore(db)
  const knownHosts = new KnownHosts(db)
  const audit = new AuditQueue(db)
  const policy = await loadAiPolicy(opts.dataDir, audit)
  const pool = new McpHostPool(hostStore, knownHosts, audit, policy.limits.idleCloseSec)

  const server = new McpServer({ name: 'bgshell', version: opts.version ?? '0.1.0' })
  registerTools(server, { hostStore, knownHosts, audit, policy, pool })

  const transport = new StdioServerTransport()
  await server.connect(transport)

  // 此时客户端 initialize 尚未完成（getClientVersion 不可用），client 标识记录在每条 ai.tool 事件中
  await audit.append('ai.server-started', {
    policy: {
      execEnabled: policy.exec.enabled,
      execDefault: policy.exec.defaultAction,
      writeEnabled: policy.write.enabled,
      writeDefault: policy.write.defaultAction,
    },
  })
  process.stderr.write(
    `[bgshell-mcp] ready (exec=${policy.exec.defaultAction}, write=${policy.write.defaultAction})\n`,
  )

  // stdio 关闭（客户端退出）→ 关闭全部连接池并退出进程
  server.server.onclose = () => {
    pool.closeAll()
    void audit.append('ai.server-stopped', {}).then(
      () => process.exit(0),
      () => process.exit(0),
    )
  }
}
