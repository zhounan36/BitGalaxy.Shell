import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { Key, ReactNode } from 'react'
import { Button, Empty, Menu, Space, Spin, Tooltip, Tree, message } from 'antd'
import type { MenuProps } from 'antd'
import {
  AppstoreOutlined,
  CopyOutlined,
  DatabaseOutlined,
  DeleteOutlined,
  FieldBinaryOutlined,
  FileAddOutlined,
  LinkOutlined,
  PlusOutlined,
  PoweroffOutlined,
  SearchOutlined,
  ShrinkOutlined,
  TableOutlined,
  ReloadOutlined,
} from '@ant-design/icons'
import type { SqlConnRecord, SqlRequest, SqlResult } from '@bgshell/protocol'
import { writeClipboardText } from '../api/clipboard'
import { describeSqlError, qualifiedTable } from './sqlTypes'

type NodeKind = 'conn' | 'db' | 'table' | 'columns' | 'root'
// 'root' 仅作空白区右键菜单的标记 key，不对应真实树节点

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
  if (kind === 'conn') {
    // conn:{savedId}（未连接）或 conn:{savedId}@{runtimeConnId}（活跃）。
    // 活跃节点每次连接换 key：让 rc-tree 的懒加载缓存失效，重连后能重新拉数据库列表
    const i = body.indexOf('@')
    return { kind: 'conn', name: i >= 0 ? body.slice(0, i) : body }
  }
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

const connKey = (savedId: string, connId?: string) =>
  connId ? `conn:${savedId}@${connId}` : `conn:${savedId}`
const dbKey = (db: string) => `db:${db}`
const tableKey = (schema: string, table: string) => `tbl:${schema}/${table}`
const colsKey = (schema: string, table: string) => `cols:${schema}/${table}`

interface MenuState {
  x: number
  y: number
  key: string
}

/** 连接节点标题：活跃=高亮加粗；未连接=灰显 +「双击连接」提示；连接中=spinner */
function connTitle(rec: SqlConnRecord, isActive: boolean, connecting: boolean) {
  return (
    <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6, minWidth: 0 }}>
      {connecting && <Spin size="small" />}
      <span
        style={{
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
          ...(isActive ? { fontWeight: 600 } : { color: 'var(--text-muted)' }),
        }}
      >
        {rec.name}
      </span>
      {!isActive && !connecting && (
        <span style={{ color: 'var(--text-muted)', fontSize: 11, opacity: 0.75, flexShrink: 0 }}>
          双击连接
        </span>
      )}
    </span>
  )
}

/** 连接列表树（Navicat/DBeaver 风格）：所有已保存连接常驻左侧，
 *  断开只收起/灰显、不消失；未连接的连接双击即连（切换活跃连接）；
 *  活跃连接 → databases → 表 → 字段逐级懒加载。
 *  双击表节点 = 打开表数据页签；右键菜单按节点类型提供
 *  连接/断开/删除、打开数据/查看结构/生成 SELECT/新建查询/刷新等。 */
export function SqlSchemaTree({
  savedConns,
  connId,
  activeSavedId,
  connectingSavedId,
  dbType,
  request,
  onRunSql,
  onOpenTable,
  onNewQuery,
  onConnectSaved,
  onDisconnectSaved,
  onDeleteSaved,
  onNewConn,
  onError,
  refreshTick,
}: {
  /** 已保存连接档案（sql-connections.db），常驻列表 */
  savedConns: SqlConnRecord[]
  /** 活跃连接的运行时会话 id（introspect 用）；未连接为 undefined */
  connId?: string
  /** 活跃连接对应的已保存档案 id（未连接或连接未保存为 undefined） */
  activeSavedId?: string
  /** 正在连接中的档案 id（节点显示 spinner） */
  connectingSavedId?: string
  dbType: string
  request: (op: SqlRequest, timeoutMs?: number) => Promise<SqlResult>
  /** 右键「生成 SELECT」：生成查询并执行 */
  onRunSql: (sql: string) => void
  /** 双击表 / 右键「打开数据」「查看结构」：打开表页签 */
  onOpenTable: (schema: string | undefined, table: string, viewKind: string | undefined, target: 'data' | 'struct') => void
  /** 右键库节点「新建查询」：新建查询页签 */
  onNewQuery?: () => void
  /** 未连接连接的双击/右键「连接」：切换活跃连接 */
  onConnectSaved: (rec: SqlConnRecord) => void
  /** 活跃连接右键「断开连接」 */
  onDisconnectSaved: () => void
  /** 右键「删除连接」（含二次确认，由父组件负责） */
  onDeleteSaved: (rec: SqlConnRecord) => void
  /** 空白区右键「新建连接」/ 空态按钮 */
  onNewConn: () => void
  onError: (title: string, hint?: string) => void
  /** 外部触发刷新（连接建立后重置树） */
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

  const activeKey = activeSavedId && connId ? connKey(activeSavedId, connId) : undefined

  // 列表 → 树：所有已保存连接常驻；活跃连接可展开（子树懒加载），未连接的灰显为叶子。
  // activeSavedId / connId / refreshTick 变化 = 连接切换：重置展开与选中；
  // 仅列表变化（新增/删除其他连接）时保留活跃节点已加载的子树与展开状态。
  const prevCtxRef = useRef<{ active?: string; connId?: string; tick: number } | undefined>(undefined)
  useEffect(() => {
    const prev = prevCtxRef.current
    const ctxChanged =
      !prev || prev.active !== activeSavedId || prev.connId !== connId || prev.tick !== refreshTick
    prevCtxRef.current = { active: activeSavedId, connId, tick: refreshTick }
    setTreeData((prevTree) => {
      const prevActive =
        !ctxChanged && activeKey ? prevTree.find((n) => n.key === activeKey) : undefined
      return savedConns.map((rec) => {
        const isActive = !!activeKey && rec.id === activeSavedId
        return {
          key: isActive ? activeKey : connKey(rec.id),
          title: connTitle(rec, isActive, rec.id === connectingSavedId),
          icon: <DatabaseOutlined style={isActive ? { color: 'var(--accent)' } : undefined} />,
          ...(isActive
            ? prevActive?.children
              ? { children: prevActive.children }
              : {}
            : { isLeaf: true }),
        }
      })
    })
    if (ctxChanged) {
      setExpandedKeys([])
      setSelectedKeys([])
      tableKindRef.current.clear()
    }
  }, [savedConns, activeSavedId, connId, connectingSavedId, refreshTick, activeKey])

  /** 加载某节点的子节点并写回树 */
  const loadChildren = useCallback(
    async (key: string, parentTree: TreeNode[]): Promise<TreeNode[]> => {
      if (!connId) return parentTree
      const meta = metaRef.current(key)
      if (meta.kind === 'conn') {
        // 活跃连接节点首次展开：拉数据库列表（懒加载入口）
        const r = await request({ op: 'introspect', connId, scope: 'databases' })
        if (!r.ok) {
          const e = describeSqlError(r.errorKind, r.message)
          onError(e.title, e.hint)
          return parentTree
        }
        return parentTree.map((n) =>
          n.key === key
            ? {
                ...n,
                children: (r.databases ?? []).map((d) => ({
                  key: dbKey(d.name),
                  title: d.name,
                  icon: <DatabaseOutlined />,
                })),
              }
            : n,
        )
      }
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
    [connId, findNode, loadChildren, treeData, updateNodeChildren],
  )

  /** 刷新活跃连接节点（重拉数据库列表）；头部刷新按钮与空白区右键共用 */
  const reloadActive = useCallback(() => {
    if (activeKey) void reloadNode(activeKey)
  }, [activeKey, reloadNode])

  /** antd 原生懒加载：无 children 的节点渲染为可展开（箭头），展开时拉取子节点
   *  （连接→databases、db→tables、table→columns，全部点击后才取数）。
   *  不传 loadData 时 antd 把无 children 节点一律当叶子（无箭头），展开懒加载永不触发。 */
  const loadData = useCallback(
    (node: { key?: Key }): Promise<void> => {
      const key = String(node.key ?? '')
      const meta = metaRef.current(key)
      if (meta.kind === 'columns') return Promise.resolve()
      // 未连接的连接节点是叶子（isLeaf），不会触发；防御性兜底
      if (meta.kind === 'conn' && key !== activeKey) return Promise.resolve()
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
    [findNode, loadChildren, treeData, updateNodeChildren, activeKey],
  )

  const onExpand = useCallback((keys: Key[]) => {
    setExpandedKeys(keys)
  }, [])

  /** 双击：未连接的连接节点 = 连接（切换活跃连接）；表节点 = 打开数据页签（Navicat 行为）；
   *  活跃连接/库节点的双击展开收起由 expandAction="doubleClick" 接管（展开即懒加载） */
  const onDoubleClick = useCallback(
    (_: unknown, node: unknown) => {
      const key = String((node as { key?: Key }).key ?? '')
      const meta = metaRef.current(key)
      if (meta.kind === 'conn') {
        if (key === activeKey) return
        const rec = savedConns.find((r) => r.id === meta.name)
        if (rec && rec.id !== connectingSavedId) onConnectSaved(rec)
        return
      }
      if (meta.kind !== 'table') return
      onOpenTable(meta.schema, meta.name, tableKindRef.current.get(key), 'data')
    },
    [activeKey, savedConns, connectingSavedId, onConnectSaved, onOpenTable],
  )

  const onRightClick = useCallback((info: unknown) => {
    const e = (info as { event: MouseEvent; node: { key?: Key } }).event
    const key = String((info as { node: { key?: Key } }).node?.key ?? 'root')
    e.preventDefault()
    setMenu({ x: e.clientX, y: e.clientY, key })
  }, [])

  /** 右键菜单按节点类型生成：
   *  连接（未连接）：连接/删除；连接（活跃）：断开/删除；
   *  表：打开数据/查看结构/生成 SELECT/复制 SELECT/复制表名；
   *  库：新建查询/复制库名；列：复制列名；空白：新建连接/刷新/全部收起 */
  const menuItems: MenuProps['items'] = useMemo(() => {
    if (!menu) return []
    const meta = metaRef.current(menu.key)
    if (meta.kind === 'conn') {
      const rec = savedConns.find((r) => r.id === meta.name)
      if (!rec) return []
      const isActive = menu.key === activeKey
      return [
        ...(isActive
          ? [{ key: 'conn-disconnect', label: '断开连接', icon: <PoweroffOutlined /> }]
          : [{ key: 'conn-connect', label: '连接', icon: <LinkOutlined /> }]),
        { type: 'divider' },
        { key: 'conn-delete', label: '删除连接', icon: <DeleteOutlined />, danger: true },
      ]
    }
    if (meta.kind === 'table') {
      return [
        { key: 'open-data', label: '打开数据', icon: <TableOutlined /> },
        { key: 'open-struct', label: '查看结构', icon: <AppstoreOutlined /> },
        { type: 'divider' },
        { key: 'gen-select', label: '生成 SELECT', icon: <SearchOutlined /> },
        { key: 'copy-sql', label: '复制 SELECT 语句', icon: <CopyOutlined /> },
        { key: 'copy-name', label: '复制表名', icon: <CopyOutlined /> },
        { type: 'divider' },
        { key: 'refresh', label: '刷新', icon: <ReloadOutlined /> },
      ]
    }
    if (meta.kind === 'db') {
      return [
        { key: 'new-query', label: '新建查询', icon: <FileAddOutlined /> },
        { type: 'divider' },
        { key: 'copy-name', label: '复制库名', icon: <CopyOutlined /> },
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
    // 空白区（连接列表层）
    return [
      { key: 'new-conn', label: '新建连接', icon: <PlusOutlined /> },
      { type: 'divider' },
      { key: 'refresh', label: '刷新', icon: <ReloadOutlined /> },
      { key: 'collapse-all', label: '全部收起', icon: <ShrinkOutlined /> },
    ]
  }, [menu, activeKey, savedConns])

  const onMenuClick: MenuProps['onClick'] = ({ key, domEvent }) => {
    domEvent.stopPropagation()
    const menuKey = menu?.key
    setMenu(undefined)
    if (!menuKey) return
    const meta = metaRef.current(menuKey)
    if (key === 'conn-connect' && meta.kind === 'conn') {
      const rec = savedConns.find((r) => r.id === meta.name)
      if (rec) onConnectSaved(rec)
      return
    }
    if (key === 'conn-disconnect') {
      onDisconnectSaved()
      return
    }
    if (key === 'conn-delete' && meta.kind === 'conn') {
      const rec = savedConns.find((r) => r.id === meta.name)
      if (rec) onDeleteSaved(rec)
      return
    }
    if (key === 'new-conn' && meta.kind === 'root') {
      onNewConn()
      return
    }
    if (key === 'refresh') {
      if (meta.kind === 'root') reloadActive()
      else void reloadNode(menuKey)
      return
    }
    if (key === 'collapse-all') {
      setExpandedKeys([])
      return
    }
    if (key === 'refresh-parent' && meta.kind === 'columns') {
      // 列节点刷新 = 重载父表（重新拉列定义）
      void reloadNode(tableKey(meta.schema ?? '', meta.table ?? meta.name))
      return
    }
    if (key === 'new-query' && meta.kind === 'db') {
      onNewQuery?.()
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
      if (key === 'copy-sql' && meta.kind === 'table') {
        const from = qualifiedTable(meta.schema, meta.name, dbType)
        void writeClipboardText(`SELECT * FROM ${from} LIMIT 100`).then(
          () => message.success('已复制 SELECT 语句'),
          () => message.error('复制失败'),
        )
        return
      }
      if (key === 'copy-name') {
        void writeClipboardText(meta.name).then(
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
        <span>连接</span>
        <Space size={6}>
          <Tooltip title="新建连接">
            <PlusOutlined
              style={{ cursor: 'pointer', color: 'var(--text-muted)' }}
              onClick={onNewConn}
            />
          </Tooltip>
          <Tooltip title="刷新">
            <ReloadOutlined
              style={{ cursor: 'pointer', color: 'var(--text-muted)' }}
              onClick={reloadActive}
            />
          </Tooltip>
        </Space>
      </div>
      {/* 空白区右键（树节点右键由 Tree onRightClick 处理，此处只接管非节点区域） */}
      <div
        style={{ flex: 1, overflow: 'auto', padding: '4px 0' }}
        onContextMenu={(e) => {
          const target = e.target as HTMLElement
          if (target.closest('.ant-tree-treenode')) return
          e.preventDefault()
          setMenu({ x: e.clientX, y: e.clientY, key: 'root' })
        }}
      >
        <Spin spinning={loading}>
          {savedConns.length === 0 ? (
            <Empty
              image={Empty.PRESENTED_IMAGE_SIMPLE}
              description="暂无已保存连接"
              style={{ marginTop: 32 }}
            >
              <Button size="small" type="primary" icon={<PlusOutlined />} onClick={onNewConn}>
                新建连接
              </Button>
            </Empty>
          ) : (
            <Tree
              showIcon
              blockNode
              expandAction="doubleClick"
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
