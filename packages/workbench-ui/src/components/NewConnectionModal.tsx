import { useState } from 'react'
import { Empty, List, Modal, Typography } from 'antd'
import {
  ArrowLeftOutlined,
  CloudServerOutlined,
  DatabaseOutlined,
  RightOutlined,
  UsbOutlined,
} from '@ant-design/icons'
import type { HostRecord } from '@bgshell/protocol'

/** 连接类型定义（统一入口三选一） */
const CONN_KINDS = [
  {
    key: 'ssh',
    icon: <CloudServerOutlined style={{ fontSize: 22, color: 'var(--accent)' }} />,
    title: 'SSH 主机',
    desc: '远程终端 / 文件管理 / 运行状态，档案持久保存',
  },
  {
    key: 'serial',
    icon: <UsbOutlined style={{ fontSize: 22, color: 'var(--accent)' }} />,
    title: '串口设备',
    desc: '本机 COM 口透传到终端，支持 Hex 监视与信号控制',
  },
  {
    key: 'sql',
    icon: <DatabaseOutlined style={{ fontSize: 22, color: 'var(--accent)' }} />,
    title: 'SQL 数据库',
    desc: 'MySQL / PostgreSQL / SQLite，经 SSH 主机隧道连接',
  },
] as const

/** 统一「新建连接」面板：SSH 主机 / 串口设备 / SQL 数据库三类入口合一。
 *  SQL 分支先选择 SSH 隧道主机（数据库连接经该主机的隧道转发），再进入数据库面板。 */
export function NewConnectionModal({
  open,
  hosts,
  onClose,
  onAddHost,
  onOpenSerial,
  onSelectSqlHost,
}: {
  open: boolean
  hosts: HostRecord[]
  onClose: () => void
  /** 选择 SSH 主机：打开主机档案编辑抽屉 */
  onAddHost: () => void
  /** 选择串口设备：打开串口连接弹窗（含端口列表刷新） */
  onOpenSerial: () => void
  /** 选择 SQL 主机：经该 SSH 主机打开数据库面板 */
  onSelectSqlHost: (host: HostRecord) => void
}) {
  // sql 分支停留在主机选择子列表（返回回到类型列表）
  const [sqlPick, setSqlPick] = useState(false)

  const close = () => {
    setSqlPick(false)
    onClose()
  }

  return (
    <Modal
      open={open}
      title={sqlPick ? 'SQL 数据库 · 选择 SSH 隧道主机' : '新建连接'}
      footer={null}
      width={480}
      onCancel={close}
      destroyOnHidden
    >
      {sqlPick ? (
        <>
          <List
            size="small"
            dataSource={hosts}
            renderItem={(host) => (
              <List.Item
                className="host-item"
                onClick={() => {
                  setSqlPick(false)
                  onSelectSqlHost(host)
                }}
                style={{ cursor: 'pointer', padding: '10px 12px', borderRadius: 6 }}
              >
                <List.Item.Meta
                  title={host.name}
                  description={`${host.username}@${host.host}:${host.port}`}
                />
                <RightOutlined style={{ color: 'var(--text-muted)' }} />
              </List.Item>
            )}
          />
          {hosts.length === 0 && (
            <Empty
              image={Empty.PRESENTED_IMAGE_SIMPLE}
              description="暂无 SSH 主机，请先添加主机档案"
              style={{ marginTop: 16, marginBottom: 8 }}
            />
          )}
          <Typography.Link
            style={{ display: 'inline-block', marginTop: 8, fontSize: 12 }}
            onClick={() => setSqlPick(false)}
          >
            <ArrowLeftOutlined /> 返回选择连接类型
          </Typography.Link>
        </>
      ) : (
        <List
          dataSource={[...CONN_KINDS]}
          renderItem={(k) => (
            <List.Item
              style={{ cursor: 'pointer', padding: '12px 12px', borderRadius: 6 }}
              onClick={() => {
                if (k.key === 'ssh') onAddHost()
                else if (k.key === 'serial') onOpenSerial()
                else setSqlPick(true)
              }}
            >
              <List.Item.Meta avatar={k.icon} title={k.title} description={k.desc} />
              <RightOutlined style={{ color: 'var(--text-muted)', marginTop: 12 }} />
            </List.Item>
          )}
        />
      )}
    </Modal>
  )
}
