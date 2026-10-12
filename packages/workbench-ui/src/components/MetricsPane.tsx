import { useEffect, useState } from 'react'
import { Alert, Card, Col, Progress, Row, Spin, Statistic, Table, Tag } from 'antd'
import type { MetricsSample } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { formatBytes, formatUptime } from '../utils/format'

/** 运行状态面板：连通建连后 10s 周期采集（uptime/负载/内存/磁盘/Top 进程） */
export function MetricsPane({ sessionId }: { sessionId: string }) {
  const [sample, setSample] = useState<MetricsSample>()
  const [error, setError] = useState<string>()

  useEffect(() => {
    let started = false
    const off = agentSocket.onControl((frame) => {
      if (frame.t === 'session-opened' && frame.s === sessionId && !started) {
        started = true
        agentSocket.sendControl({ t: 'metrics', s: sessionId, m: { intervalSec: 10 } })
      }
      if (frame.t === 'metrics-data' && frame.s === sessionId) {
        setSample(frame.m)
        setError(undefined)
      }
      if (frame.t === 'error' && frame.s === sessionId) setError(frame.m.message)
    })
    return off
  }, [sessionId])

  if (!sample) {
    return (
      <div style={{ height: '100%', background: '#fff', display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
        {error ? <Alert type="error" showIcon message={error} /> : <Spin tip="连接主机并采集中..." />}
      </div>
    )
  }

  const memPct = sample.memTotalB > 0 ? Math.floor((sample.memUsedB / sample.memTotalB) * 100) : 0

  return (
    <div style={{ height: '100%', overflow: 'auto', background: '#f5f5f5', padding: 12 }}>
      {error && <Alert type="warning" showIcon message={error} style={{ marginBottom: 8 }} />}
      <Row gutter={[8, 8]}>
        <Col span={6}>
          <Card size="small">
            <Statistic title="主机名" value={sample.hostname} valueStyle={{ fontSize: 18 }} />
          </Card>
        </Col>
        <Col span={6}>
          <Card size="small">
            <Statistic title="内核" value={sample.kernel || '—'} valueStyle={{ fontSize: 18 }} />
          </Card>
        </Col>
        <Col span={6}>
          <Card size="small">
            <Statistic title="运行时长" value={formatUptime(sample.uptimeSec)} valueStyle={{ fontSize: 18 }} />
          </Card>
        </Col>
        <Col span={6}>
          <Card size="small">
            <Statistic
              title="负载（1/5/15 分钟）"
              value={`${sample.load1.toFixed(2)} / ${sample.load5.toFixed(2)} / ${sample.load15.toFixed(2)}`}
              valueStyle={{ fontSize: 18 }}
            />
          </Card>
        </Col>
      </Row>

      <Row gutter={[8, 8]} style={{ marginTop: 8 }}>
        <Col span={12}>
          <Card
            size="small"
            title="内存"
            extra={<Tag>{formatBytes(sample.memUsedB)} / {formatBytes(sample.memTotalB)}</Tag>}
          >
            <Progress percent={memPct} status={memPct > 85 ? 'exception' : 'normal'} />
          </Card>
        </Col>
        <Col span={12}>
          <Card size="small" title="磁盘使用">
            {sample.disks.length === 0 && '（无数据）'}
            {sample.disks.map((d) => (
              <div key={`${d.source}:${d.target}`} style={{ marginBottom: 4 }}>
                <span style={{ fontSize: 12, color: '#666' }}>{d.target}</span>
                <Progress
                  size="small"
                  percent={d.usedPct}
                  status={d.usedPct > 90 ? 'exception' : 'normal'}
                  format={() => `${d.usedPct}% · ${formatBytes(d.usedB)}/${formatBytes(d.sizeB)}`}
                />
              </div>
            ))}
          </Card>
        </Col>
      </Row>

      <Card size="small" title="Top 进程（CPU）" style={{ marginTop: 8 }}>
        <Table
          size="small"
          rowKey="pid"
          pagination={false}
          dataSource={sample.processes}
          columns={[
            { title: 'PID', dataIndex: 'pid', width: 90 },
            { title: 'CPU %', dataIndex: 'cpuPct', width: 100 },
            { title: 'MEM %', dataIndex: 'memPct', width: 100 },
            { title: '命令', dataIndex: 'command' },
          ]}
        />
      </Card>
      <div style={{ color: '#999', fontSize: 12, marginTop: 8, textAlign: 'right' }}>
        采样时间：{new Date(sample.ts).toLocaleTimeString()}（10s 自动刷新）
      </div>
    </div>
  )
}
