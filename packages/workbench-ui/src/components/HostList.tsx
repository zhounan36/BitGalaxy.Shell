import { Button, Dropdown, Empty, List, Tag } from 'antd'
import { CodeOutlined, DatabaseOutlined, DashboardOutlined, FolderOutlined, MoreOutlined, PlusOutlined, ReloadOutlined } from '@ant-design/icons'
import type { HostRecord } from '@bgshell/protocol'

export type HostAction = 'shell' | 'sftp' | 'metrics' | 'sql' | 'edit' | 'delete'

export interface HostStatus {
  ok: boolean
  ms?: number
  message?: string
}

export interface HostListProps {
  hosts: HostRecord[]
  hostStatus: Record<string, HostStatus | undefined>
  onAction: (host: HostRecord, action: HostAction) => void
  onAdd: () => void
  onRefresh: () => void
}

/** 左侧主机列表：单击开终端，菜单含 文件/状态/编辑/删除；每台主机带探测徽标（嵌入 AgentControls 容器内使用，不再自带底部状态栏） */
export function HostList({ hosts, hostStatus, onAction, onAdd, onRefresh }: HostListProps) {
  return (
    <div className="host-panel">
      <div className="host-toolbar">
        <Button type="primary" icon={<PlusOutlined />} block onClick={onAdd}>
          新建连接
        </Button>
        <Button icon={<ReloadOutlined />} onClick={onRefresh} />
      </div>
      <div className="host-list">
        {hosts.length === 0 ? (
          <Empty style={{ marginTop: 48 }} description="暂无主机档案" />
        ) : (
          <List
            dataSource={hosts}
            renderItem={(host) => {
              const st = hostStatus[host.id]
              return (
                <List.Item
                  className="host-item"
                  onClick={() => onAction(host, 'shell')}
                  actions={[
                    <Dropdown
                      key="more"
                      menu={{
                        items: [
                          { key: 'shell', label: '连接终端', icon: <CodeOutlined /> },
                          { key: 'sftp', label: '文件管理', icon: <FolderOutlined /> },
                          { key: 'sql', label: '数据库', icon: <DatabaseOutlined /> },
                          { key: 'metrics', label: '运行状态', icon: <DashboardOutlined /> },
                          { type: 'divider' as const },
                          { key: 'edit', label: '编辑' },
                          { key: 'delete', label: '删除', danger: true },
                        ],
                        onClick: ({ key, domEvent }) => {
                          domEvent.stopPropagation()
                          onAction(host, key as HostAction)
                        },
                      }}
                    >
                      <Button type="text" size="small" icon={<MoreOutlined />} onClick={(e) => e.stopPropagation()} />
                    </Dropdown>,
                  ]}
                >
                  <List.Item.Meta
                    title={
                      <span>
                        {host.name}{' '}
                        {st ? (
                          <Tag color={st.ok ? 'green' : 'red'} style={{ marginLeft: 4 }}>
                            {st.ok ? `可达 ${st.ms}ms` : '不可达'}
                          </Tag>
                        ) : (
                          <Tag style={{ marginLeft: 4 }}>探测中</Tag>
                        )}
                      </span>
                    }
                    description={`${host.username}@${host.host}:${host.port}`}
                  />
                </List.Item>
              )
            }}
          />
        )}
      </div>
    </div>
  )
}
