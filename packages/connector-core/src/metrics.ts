import type { MetricsDisk, MetricsProcess, MetricsSample } from '@bgshell/protocol'

const MARK = (name: string) => `==BGSH:${name}==`

/**
 * 单次 exec 拉取全部指标（POSIX/Linux 目标）。
 * 各段以 ==BGSH:<name>== 标记分隔；无法解析的段安全降级为 0/空。
 */
export function buildMetricsCommand(): string {
  return [
    `echo ${MARK('HOSTNAME')}; hostname`,
    `echo ${MARK('UNAME')}; uname -sr`,
    `echo ${MARK('UPTIME')}; cat /proc/uptime 2>/dev/null || echo '0 0'`,
    `echo ${MARK('LOAD')}; cat /proc/loadavg 2>/dev/null || echo '0 0 0 0/0 0'`,
    `echo ${MARK('MEM')}; free -b 2>/dev/null | awk 'NR==2{print $2,$3}' || echo '0 0'`,
    `echo ${MARK('DF')}; df -B1 -P 2>/dev/null | tail -n +2 || true`,
    `echo ${MARK('PS')}; ps -eo pid,pcpu,pmem,comm --sort=-pcpu 2>/dev/null | head -n 11 || true`,
    `echo ${MARK('END')}`,
  ].join('; ')
}

function section(out: string, name: string): string {
  const re = new RegExp(`${escapeRegex(MARK(name))}\\r?\\n([\\s\\S]*?)\\r?\\n?==BGSH:`)
  const m = out.match(re)
  return (m?.[1] ?? '').trim()
}

export function parseMetrics(stdout: string): MetricsSample {
  const load = section(stdout, 'LOAD').split(/\s+/)
  const mem = section(stdout, 'MEM').split(/\s+/)
  const disks: MetricsDisk[] = section(stdout, 'DF')
    .split('\n')
    .filter(Boolean)
    .map((line) => {
      const c = line.trim().split(/\s+/)
      return {
        source: c[0] ?? '',
        target: c[5] ?? '',
        sizeB: Number(c[1] ?? 0),
        usedB: Number(c[2] ?? 0),
        usedPct: Number((c[4] ?? '0%').replace('%', '') || 0),
      }
    })
    .filter((d) => d.target)
  const processes: MetricsProcess[] = section(stdout, 'PS')
    .split('\n')
    .filter(Boolean)
    .slice(1)
    .map((line) => {
      const c = line.trim().split(/\s+/)
      return {
        pid: c[0] ?? '',
        cpuPct: Number(c[1] ?? 0),
        memPct: Number(c[2] ?? 0),
        command: c.slice(3).join(' '),
      }
    })

  return {
    hostname: section(stdout, 'HOSTNAME').split('\n')[0] || 'unknown',
    kernel: section(stdout, 'UNAME').split('\n')[0] || '',
    uptimeSec: Math.round(Number(section(stdout, 'UPTIME').split(/\s+/)[0] || 0)),
    load1: Number(load[0] ?? 0),
    load5: Number(load[1] ?? 0),
    load15: Number(load[2] ?? 0),
    memTotalB: Number(mem[0] ?? 0),
    memUsedB: Number(mem[1] ?? 0),
    disks,
    processes,
    ts: new Date().toISOString(),
  }
}

function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}
