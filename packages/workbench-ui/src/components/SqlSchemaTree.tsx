import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { Key, ReactNode } from 'react'
import { Empty, Menu, Spin, Tooltip, Tree, message } from 'antd'
import type { MenuProps } from 'antd'
import {
  AppstoreOutlined,
  CopyOutlined,
  DatabaseOutlined,
  FieldBinaryOutlined,
  SearchOutlined,
  TableOutlined,
  ReloadOutlined,
} from '@ant-design/icons'
import type { SqlRequest, SqlResult } from '@bgshell/protocol'
import { describeSqlError, qualifiedTable } from './sqlTypes'

type NodeKind = 'root' | 'db' | 'table' | 'columns'

interface NodeMeta {
  kind: NodeKind
  name: string
  /** 表/列节点所属 database（= schema） */
  schema?: string
  table?: string
}

interface TreeNode {
  key: string
  title: ReactNode
  icon?: ReactNode
  isLeaf?: boolean
  children?: TreeNode[]
}

const metaOf = (key: string): NodeMeta => {
  const [kind, ...rest] = key.split(':')
  const body = rest.join(':')
  if (kind === 'tbl') {
    const i = body.indexOf('/')
    return { kind: 'table', schema: body.slice(0, i), name: body.slice(i + 1) }
  }
  if (kind === 'cols') {
    // cols:{schema}/{table}:{field}
    const i = body.indexOf('/')
    const rest = body.slice(i + 1)
    const j = rest.indexOf(':')
    const table = j >= 0 ? rest.slice(0, j) : rest
    return { kind: 'columns', schema: body.slice(0, i), table, name: j >= 0 ? rest.slice(j + 1) : rest }
  }
  return { kind: kind as NodeKind, name: body }
}

const dbKey = (db: string) => `db:${db}`
const tableKey = (schema: string, table: string) => `tbl:${schema}/${table}`
const colsKey = (schema: string, table: string) => `cols:${schema}/${table}`

interface MenuState {
  x: number
  y: number
  key: string
}

/** 结构树（懒加载）：连接 → databases → 表 → 字段。
 *  双击表节点 = 打开表数据页签（主流 SQL 工具行为）；右键菜单提供
 *  打开数据 / 查看结构 / 生成 SELECT / 复制名称 / 刷新。 */
export function SqlSchemaTree({
  connId,
  connLabel,
  dbType,
  request,
  onRunSql,
  onOpenTable,
  onError,
  refreshTick,
}: {
  connId?: string
  connLabel: string
  dbType: string
  request: (op: SqlRequest, timeoutMs?: number) => Promise<SqlResult>
  /** 右键「生成 SELECT」：生成查询并执行 */
  onRunSql: (sql: string) => void
  /** 双击表 / 右键「打开数据」「查看结构」：打开表页签 */
  onOpenTable: (schema: string | undefined, table: string, viewKind: string | undefined, target: 'data' | 'struct') => void
  onError: (title: string, hint?: string) => void
  /** 外部触发刷新根节点（连接栏刷新按钮） */
  refreshTick: number
}) {
  const [treeData, setTreeData] = useState<TreeNode[]>([])
  const [expandedKeys, setExpandedKeys] = useState<Key[]>([])
  const [selectedKeys, setSelectedKeys] = useState<Key[]>([])
  const [loading, setLoading] = useState(false)
  const [menu, setMenu] = useState<MenuState>()
  const metaRef = useRef(metaOf)
  /** 表节点 → kind（table | view）：双击/右键打开表页签时透传 */
  const tableKindRef = useRef(new Map<string, string | undefined>())

  // 连接切换：重置树，加载 databases
  const loadRoot = useCallback(async () => {
    if (!connId) {
      setTreeData([])
      setExpandedKeys([])
      return
    }
    setLoading(true)
    try {
      const r = await request({ op: 'introspect', connId, scope: 'databases' })
      if (!r.ok) {
        onError(describeSqlError(r.errorKind, r.message).title, describeSqlError(r.errorKind, r.message).hint)
        return
      }
      const dbs = r.databases ?? []
      setTreeData(
        dbs.map((d) => ({
          key: dbKey(d.name),
          title: d.name,
          icon: <DatabaseOutlined />,
        })),
      )
      tableKindRef.current.clear()
    } finally {
      setLoading(false)
    }
  }, [connId, request, onError])

  useEffect(() => {
    void loadRoot()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connId, refreshTick])

  /** 加载某节点的子节点并写回树 */
  const loadChildren = useCallback(
    async (key: string, parentTree: TreeNode[]): Promise<TreeNode[]> => {
      if (!connId) return parentTree
      const meta = metaRef.current(key)
      if (meta.kind === 'db') {
        const r = await request({ op: 'introspect', connId, scope: 'tables', schema: meta.name })
        if (!r.ok) {
          const e = describeSqlError(r.errorKind, r.message)
          onError(e.title, e.hint)
          return parentTree
        }
        return parentTree.map((n) =>
          n.key === key
            ? {
                ...n,
                children: (r.tables ?? []).map((t) => {
                  const k = tableKey(meta.name, t.name)
                  tableKindRef.current.set(k, t.kind)
                  return {
                    key: k,
                    title: (
                      <span>
                        {t.name}
                        {t.kind === 'view' && (
                          <span style={{ color: 'var(--text-muted)', fontSize: 11, marginLeft: 6 }}>
                            view
                          </span>
                        )}
                      </span>
                    ),
                    icon: <TableOutlined />,
                  }
                }),
              }
            : n,
        )
      }
      if (meta.kind === 'table') {
        const r = await request({ op: 'introspect', connId, scope: 'columns', schema: meta.schema, table: meta.name })
        if (!r.ok) {
          const e = describeSqlError(r.errorKind, r.message)
          onError(e.title, e.hint)
          return parentTree
        }
        return parentTree.map((n) =>
          n.key === key
            ? {
                ...n,
                children: (r.fields ?? []).map((f) => ({
                  key: `${colsKey(meta.schema ?? '', meta.name)}:${f.name}`,
                  isLeaf: true,
                  title: (
                    <span>
                      {f.name}
                      <span style={{ color: 'var(--text-muted)', fontSize: 11, marginLeft: 6 }}>
                        {f.dataType ?? ''}
                        {f.pk ? ' PK' : ''}
                      </span>
                    </span>
                  ),
                  icon: <FieldBinaryOutlined />,
                })),
              }
            : n,
        )
      }
      return parentTree
    },
    [connId, request, onError],
  )

  /** 树内更新子节点（含嵌套 db 节点） */
  const updateNodeChildren = useCallback((data: TreeNode[], key: string, children: TreeNode[]): TreeNode[] =>
    data.map((n) =>
      n.key === key
        ? { ...n, children }
        : n.children
          ? { ...n, children: updateNodeChildren(n.children, key, children) }
          : n,
    ), [])

  const findNode = useCallback(
    (data: TreeNode[], key: string): TreeNode | undefined => {
      for (const n of data) {
        if (n.key === key) return n
        const hit = n.children && findNode(n.children, key)
        if (hit) return hit
      }
      return undefined
    },
    [],
  )

  const reloadNode = useCallback(
    async (key: string) => {
      if (!connId) return
      if (key === 'root') {
        void loadRoot()
        return
      }
      setLoading(true)
      try {
        const node = findNode(treeData, key)
        const next = await loadChildren(key, node ? [node] : [])
        if (next.length === 1) {
          setTreeData((prev) => updateNodeChildren(prev, key, next[0].children ?? []))
        }
      } finally {
        setLoading(false)
      }
    },
    [connId, findNode, loadChildren, loadRoot, treeData, updateNodeChildren],
  )

  /** antd 原生懒加载：无 children 的节点渲染为可展开（箭头），展开时拉取子节点。
   *  不传 loadData 时 antd 把无 children 节点一律当叶子（无箭头），展开懒加载永不触发
   *  —— 表现为「连接后点库节点无反应，右键刷新才出表」。 */
  const loadData = useCallback(
    (node: { key?: Key }): Promise<void> => {
      const key = String(node.key ?? '')
      const meta = metaRef.current(key)
      if (meta.kind === 'columns') return Promise.resolve()
      return (async () => {
        setLoading(true)
        try {
          const nd = findNode(treeData, key)
          const next = await loadChildren(key, nd ? [nd] : [])
          if (next.length === 1) {
            setTreeData((prev) => updateNodeChildren(prev, key, next[0].children ?? []))
          }
        } finally {
          setLoading(false)
        }
      })()
    },
    [findNode, loadChildren, treeData, updateNodeChildren],
  )

  const onExpand = useCallback((keys: Key[]) => {
    setExpandedKeys(keys)
  }, [])

  /** 双击表节点 = 打开数据页签（Navicat/DBeaver 行为）；双击库节点 = 展开/收起（树默认） */
  const onDoubleClick = useCallback(
    (_: unknown, node: unknown) => {
      const key = String((node as { key?: Key }).key ?? '')
      const meta = metaRef.current(key)
      if (meta.kind !== 'table') return
      onOpenTable(meta.schema, meta.name, tableKindRef.current.get(key), 'data')
    },
    [onOpenTable],
  )

  const onRightClick = useCallback((info: unknown) => {
    const e = (info as { event: MouseEvent; node: { key?: Key } }).event
    const key = String((info as { node: { key?: Key } }).node?.key ?? 'root')
    e.preventDefault()
    setMenu({ x: e.clientX, y: e.clientY, key })
  }, [])

  /** 右键菜单按节点类型生成（表节点提供打开数据/查看结构/生成 SELECT/复制表名） */
  const menuItems: MenuProps['items'] = useMemo(() => {
    if (!menu) return []
    const meta = metaRef.current(menu.key)
    if (meta.kind === 'table') {
      return [
        { key: 'open-data', label: '打开数据', icon: <TableOutlined /> },
        { key: 'open-struct', label: '查看结构', icon: <AppstoreOutlined /> },
        { type: 'divider' },
        { key: 'gen-select', label: '生成 SELECT', icon: <SearchOutlined /> },
        { key: 'copy-name', label: '复制表名', icon: <CopyOutlined /> },
        { type: 'divider' },
        { key: 'refresh', label: '刷新', icon: <ReloadOutlined /> },
      ]
    }
    if (meta.kind === 'columns') {
      return [
        { key: 'copy-name', label: '复制列名', icon: <CopyOutlined /> },
        { type: 'divider' },
        { key: 'refresh-parent', label: '刷新', icon: <ReloadOutlined /> },
      ]
    }
    return [{ key: 'refresh', label: '刷新', icon: <ReloadOutlined /> }]
  }, [menu])

  const onMenuClick: MenuProps['onClick'] = ({ key, domEvent }) => {
    domEvent.stopPropagation()
    const menuKey = menu?.key
    setMenu(undefined)
    if (!menuKey) return
    const meta = metaRef.current(menuKey)
    if (key === 'refresh') {
      void reloadNode(menuKey)
      return
    }
    if (key === 'refresh-parent' && meta.kind === 'columns') {
      // 列节点刷新 = 重载父表（重新拉列定义）
      void reloadNode(tableKey(meta.schema ?? '', meta.table ?? meta.name))
      return
    }
    if (meta.kind === 'table' || meta.kind === 'columns') {
      if (key === 'open-data') {
        onOpenTable(meta.schema, meta.name, tableKindRef.current.get(menuKey), 'data')
        return
      }
      if (key === 'open-struct') {
        onOpenTable(meta.schema, meta.name, tableKindRef.current.get(menuKey), 'struct')
        return
      }
      if (key === 'gen-select') {
        const from = qualifiedTable(meta.schema, meta.name, dbType)
        onRunSql(`SELECT * FROM ${from} LIMIT 100`)
        return
      }
      if (key === 'copy-name') {
        void navigator.clipboard.writeText(meta.name).then(
          () => message.success('已复制'),
          () => message.error('复制失败'),
        )
      }
    }
  }

  return (
    <div style={{ height: '100%', display: 'flex', flexDirection: 'column', position: 'relative' }}>
      <div
        style={{
          padding: '6px 10px',
          borderBottom: '1px solid var(--border-subtle)',
          color: 'var(--text-secondary)',
          fontSize: 12,
          display: 'flex',
          justifyContent: 'space-between',
          alignItems: 'center',
        }}
      >
        <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
          {connLabel}
        </span>
        <Tooltip title="刷新">
          <ReloadOutlined
            style={{ cursor: 'pointer', color: 'var(--text-muted)' }}
            onClick={() => void reloadNode('root')}
          />
        </Tooltip>
      </div>
      <div style={{ flex: 1, overflow: 'auto', padding: '4px 0' }}>
        <Spin spinning={loading}>
          {!connId ? (
            <Empty
              image={Empty.PRESENTED_IMAGE_SIMPLE}
              description="未连接"
              style={{ marginTop: 40 }}
            />
          ) : (
            <Tree
              showIcon
              blockNode
              treeData={treeData}
              expandedKeys={expandedKeys}
              selectedKeys={selectedKeys}
              loadData={loadData}
              onExpand={onExpand}
              onSelect={(keys) => setSelectedKeys(keys)}
              onDoubleClick={onDoubleClick}
              onRightClick={onRightClick}
            />
          )}
        </Spin>
      </div>
      {menu && (
        <>
          <div
            style={{ position: 'fixed', inset: 0, zIndex: 1049 }}
            onClick={() => setMenu(undefined)}
            onContextMenu={(e) => {
              e.preventDefault()
              setMenu(undefined)
            }}
          />
          <Menu
            items={menuItems}
            onClick={onMenuClick}
            style={{
              position: 'fixed',
              left: menu.x,
              top: menu.y,
              zIndex: 1050,
              border: '1px solid var(--border)',
            }}
          />
        </>
      )}
    </div>
  )
}
