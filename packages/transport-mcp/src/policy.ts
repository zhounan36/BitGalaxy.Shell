import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import type { AuditQueue } from '@bgshell/connector-core'

// ── 策略模型（docs/ai-design.md §6）─────────────────────────────────────────

export type PolicyAction = 'allow' | 'ask' | 'deny'

export interface AiPolicy {
  version: 1
  exec: {
    enabled: boolean
    defaultAction: PolicyAction
    /** 白名单：命中即放行（正则源串，大小写不敏感） */
    allowPatterns: string[]
    /** 用户追加黑名单（叠加到内置硬黑名单之后） */
    denyPatterns: string[]
  }
  write: {
    enabled: boolean
    defaultAction: PolicyAction
  }
  limits: {
    execTimeoutMs: number
    maxExecOutputBytes: number
    maxReadFileBytes: number
    idleCloseSec: number
  }
}

/** 内置硬黑名单：永不可关，优先于一切白名单（docs/ai-design.md §6） */
export const BUILTIN_DENY_PATTERNS: readonly string[] = [
  String.raw`\brm\s+[^;|&]*-(?:[a-zA-Z]*r[a-zA-Z]*f|[a-zA-Z]*f[a-zA-Z]*r)[a-zA-Z]*\s+(/|~)(?:\s|/|$|\*)`, // rm -rf / ~ 根删除
  String.raw`\bmkfs(?:\.\w+)?\b`, // 格式化
  String.raw`\bdd\s+[^;|&]*of=/dev/`, // dd 写块设备
  String.raw`>\s*/dev/(?:sd|nvme|hd|vd|xvd)`, // 重定向写块设备
  String.raw`\b(?:shutdown|reboot|halt|poweroff)\b`, // 关机重启
  String.raw`\binit\s+[06]\b`,
  String.raw`:\(\)\s*\{[^}]*\}\s*;\s*:`, // fork 炸弹
  String.raw`\bchmod\s+[^;|&]*777\s+/(?:\s|$)`, // 根递归放开权限
  String.raw`\bchown\s+[^;|&]*\s/(?:\s|$)[^;|&]*-R|\bchown\s+[^;|&]*-R[^;|&]*\s+/(?:\s|$)`, // 根递归改属主
  String.raw`\b(?:curl|wget)\b[^;|&]*\|\s*(?:sudo\s+)?(?:ba|z|da|k)?sh\b`, // 远程脚本管道直执行
  String.raw`\|\s*sudo\s+(?:ba)?sh\b`,
]

export function defaultPolicy(): AiPolicy {
  return {
    version: 1,
    exec: { enabled: true, defaultAction: 'ask', allowPatterns: [], denyPatterns: [] },
    write: { enabled: true, defaultAction: 'ask' },
    limits: {
      execTimeoutMs: 30_000,
      maxExecOutputBytes: 64 * 1024,
      maxReadFileBytes: 256 * 1024,
      idleCloseSec: 300,
    },
  }
}

/** 加载 ai-policy.json：缺失→默认；损坏→默认 + 审计回退事件 */
export async function loadAiPolicy(
  dataDir: string,
  audit: AuditQueue,
): Promise<AiPolicy> {
  const file = join(dataDir, 'ai-policy.json')
  try {
    const raw = JSON.parse(await readFile(file, 'utf8')) as Partial<AiPolicy>
    const def = defaultPolicy()
    return {
      version: 1,
      exec: { ...def.exec, ...raw.exec },
      write: { ...def.write, ...raw.write },
      limits: { ...def.limits, ...raw.limits },
    }
  } catch (err) {
    const code = (err as NodeJS.ErrnoException)?.code
    if (code !== 'ENOENT') {
      await audit.append('ai.policy-fallback', { reason: (err as Error).message })
    }
    return defaultPolicy()
  }
}

// ── 命令分类器（docs/ai-design.md §4.2）────────────────────────────────────

export interface CommandDecision {
  action: PolicyAction
  reason: 'deny-pattern' | 'allow-pattern' | 'default'
  /** 命中的正则（审计用） */
  pattern?: string
}

function matches(command: string, patterns: readonly string[]): string | undefined {
  for (const src of patterns) {
    try {
      if (new RegExp(src, 'i').test(command)) return src
    } catch {
      // 用户配置的非法正则：跳过（策略文件治理问题，不阻断其他规则）
    }
  }
  return undefined
}

/** 优先级：硬黑名单 > 用户黑名单 > 白名单 > defaultAction */
export function classifyCommand(command: string, policy: AiPolicy): CommandDecision {
  const denied =
    matches(command, BUILTIN_DENY_PATTERNS) ?? matches(command, policy.exec.denyPatterns)
  if (denied) return { action: 'deny', reason: 'deny-pattern', pattern: denied }
  const allowed = matches(command, policy.exec.allowPatterns)
  if (allowed) return { action: 'allow', reason: 'allow-pattern', pattern: allowed }
  return { action: policy.exec.defaultAction, reason: 'default' }
}
