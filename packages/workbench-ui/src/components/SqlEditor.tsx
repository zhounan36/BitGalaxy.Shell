import { useMemo, useRef, useState } from 'react'
import CodeMirror from '@uiw/react-codemirror'
import { EditorView, keymap } from '@codemirror/view'
import type { Extension } from '@codemirror/state'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'
import { sql } from '@codemirror/lang-sql'
import { Alert, Button, Menu, Modal, Spin, message } from 'antd'
import type { MenuProps } from 'antd'
import {
  BulbOutlined,
  ClearOutlined,
  CommentOutlined,
  CopyOutlined,
  PlayCircleOutlined,
  RocketOutlined,
  SelectOutlined,
} from '@ant-design/icons'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { EditorView as CMEditorView } from '@codemirror/view'
import { askAi } from '../api/askAi'
import { writeClipboardText } from '../api/clipboard'

type AiAction = 'explain' | 'optimize' | 'comment'

const AI_ACTION_TITLE: Record<AiAction, string> = {
  explain: 'AI 解释 SQL',
  optimize: 'AI 优化 SQL',
  comment: 'AI 注释 SQL',
}

/** 一次性问答的 system：SQL 专家角色（主程序 AiHandler 直接透传给 LLM） */
const AI_SYSTEM = '你是资深数据库专家，精通 SQL 与主流数据库方言（MySQL/PostgreSQL/SQLite 等）。回答使用中文，准确、简洁。'

interface MenuState {
  x: number
  y: number
  hasSelection: boolean
  hasText: boolean
}

interface AiState {
  action: AiAction
  sql: string
  hadSelection: boolean
  loading: boolean
  text?: string
  error?: string
}

/** SQL 编辑器（CodeMirror 6）：深色主题、行号、Ctrl+Enter 执行快捷键。
 *  右键菜单：运行/复制/全选/清空 + AI 解释 / AI 优化 / AI 注释
 *  （AI 走主程序 7777 的一次性问答通道，无需数据库连接）。 */
export function SqlEditor({
  value,
  onChange,
  onRun,
  readOnly,
  dbType,
}: {
  value: string
  onChange: (v: string) => void
  /** Ctrl+Enter（Mac 上 Cmd+Enter）：编辑器自行取选区文本一并回调 */
  onRun: (text: string, hasSelection: boolean) => void
  readOnly?: boolean
  /** 数据库类型（AI 分析时的方言上下文） */
  dbType?: string
}) {
  const viewRef = useRef<CMEditorView | null>(null)
  const [menu, setMenu] = useState<MenuState>()
  const [ai, setAi] = useState<AiState>()

  const extensions = useMemo<Extension[]>(() => {
    return [
      sql(),
      history(),
      keymap.of([
        {
          key: 'Mod-Enter',
          preventDefault: true,
          run: (view) => {
            const { state } = view
            const sel = state.selection.main
            const text = sel.empty ? state.doc.toString() : state.sliceDoc(sel.from, sel.to)
            onRun(text, !sel.empty)
            return true
          },
        },
        ...defaultKeymap,
        ...historyKeymap,
      ]),
      EditorView.theme({
        '&': { height: '100%', fontSize: '13px' },
        '.cm-scroller': { fontFamily: 'Consolas, Monaco, "Courier New", monospace' },
        '.cm-gutters': {
          background: 'var(--bg-deep)',
          color: 'var(--text-muted)',
          borderRight: '1px solid var(--border-subtle)',
        },
        '.cm-activeLineGutter': { background: 'var(--bg-raised)', color: 'var(--text-secondary)' },
        '.cm-content': { caretColor: 'var(--accent-bright)' },
        '&.cm-focused': { outline: 'none' },
      }),
    ]
  }, [onRun])

  /** 选区或全文（右键动作与 Ctrl+Enter 同语义：有选区用选区） */
  const selectionOrAll = () => {
    const view = viewRef.current
    if (!view) return { text: '', hasSelection: false }
    const sel = view.state.selection.main
    if (!sel.empty) {
      return { text: view.state.sliceDoc(sel.from, sel.to), hasSelection: true }
    }
    return { text: view.state.doc.toString(), hasSelection: false }
  }

  const onContextMenu = (e: React.MouseEvent) => {
    e.preventDefault()
    const s = selectionOrAll()
    setMenu({ x: e.clientX, y: e.clientY, hasSelection: s.hasSelection, hasText: !!s.text.trim() })
  }

  /** AI 动作提示词（方言上下文 + 只输出要求） */
  const aiPrompt = (action: AiAction, sql: string) => {
    const dialect = dbType ? `数据库类型：${dbType}。` : ''
    if (action === 'explain') {
      return `请解释下面这段 SQL：说明它做了什么、关键子句的作用，以及可能的问题或风险。${dialect}\n\n${sql}`
    }
    if (action === 'optimize') {
      return `请优化下面这段 SQL，提升可读性与执行效率（仅在明显有益时补充条件或索引提示，不得改变查询语义）。只输出优化后的 SQL 本身，不要任何解释。${dialect}\n\n${sql}`
    }
    return `请为下面这段 SQL 的关键行添加简洁的中文行内注释（-- 注释）。只输出带注释的 SQL 本身，不要任何解释。${dialect}\n\n${sql}`
  }

  const startAi = (action: AiAction) => {
    const s = selectionOrAll()
    if (!s.text.trim()) return
    setAi({ action, sql: s.text, hadSelection: s.hasSelection, loading: true })
    void askAi(aiPrompt(action, s.text), AI_SYSTEM).then((r) => {
      setAi((prev) =>
        prev
          ? {
              ...prev,
              loading: false,
              text: r.ok ? r.text : undefined,
              error: r.ok ? undefined : r.message ?? 'AI 请求失败，请重试',
            }
          : prev,
      )
    })
  }

  /** 把 AI 产出写回编辑器：替换当前选区（发起时通常选中了 SQL），无选区则插入光标处 */
  const insertAiResult = () => {
    const view = viewRef.current
    const text = ai?.text?.trim()
    if (!view || !text) return
    const sel = view.state.selection.main
    view.dispatch({
      changes: { from: sel.from, to: sel.to, insert: text },
      selection: { anchor: sel.from + text.length },
    })
    setAi(undefined)
    message.success('已写回编辑器')
  }

  const menuItems: MenuProps['items'] = useMemo(() => {
    if (!menu) return []
    return [
      {
        key: 'run',
        label: menu.hasSelection ? '运行选中' : '运行全部',
        icon: <PlayCircleOutlined />,
        disabled: !menu.hasText,
      },
      {
        key: 'copy',
        label: menu.hasSelection ? '复制选中' : '复制全部',
        icon: <CopyOutlined />,
        disabled: !menu.hasText,
      },
      { key: 'select-all', label: '全选', icon: <SelectOutlined /> },
      { key: 'clear', label: '清空', icon: <ClearOutlined />, disabled: !menu.hasText },
      { type: 'divider' },
      { key: 'ai-explain', label: 'AI 解释此 SQL', icon: <BulbOutlined />, disabled: !menu.hasText },
      { key: 'ai-optimize', label: 'AI 优化此 SQL', icon: <RocketOutlined />, disabled: !menu.hasText },
      { key: 'ai-comment', label: 'AI 注释此 SQL', icon: <CommentOutlined />, disabled: !menu.hasText },
    ]
  }, [menu])

  const onMenuClick: MenuProps['onClick'] = ({ key }) => {
    setMenu(undefined)
    const s = selectionOrAll()
    if (key === 'run') {
      onRun(s.text, s.hasSelection)
      return
    }
    if (key === 'copy') {
      void writeClipboardText(s.text).then(
        () => message.success('已复制'),
        () => message.error('复制失败'),
      )
      return
    }
    if (key === 'select-all') {
      const view = viewRef.current
      if (view) view.dispatch({ selection: { anchor: 0, head: view.state.doc.length } })
      return
    }
    if (key === 'clear') {
      onChange('')
      return
    }
    if (key === 'ai-explain') startAi('explain')
    if (key === 'ai-optimize') startAi('optimize')
    if (key === 'ai-comment') startAi('comment')
  }

  /** AI 结果是否适合写回编辑器（优化/注释产出纯 SQL；解释是说明文本） */
  const aiInsertable = ai && (ai.action === 'optimize' || ai.action === 'comment')

  return (
    <div style={{ height: '100%' }} onContextMenu={onContextMenu}>
      <CodeMirror
        value={value}
        onChange={onChange}
        onCreateEditor={(view) => {
          viewRef.current = view
        }}
        extensions={extensions}
        theme="dark"
        readOnly={readOnly}
        height="100%"
        style={{ height: '100%' }}
        basicSetup={{
          lineNumbers: true,
          foldGutter: true,
          highlightActiveLine: true,
          autocompletion: true,
        }}
      />

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

      {/* ── AI 结果弹窗：加载中 / 错误 / 文本展示 + 写回编辑器 ── */}
      <Modal
        title={ai ? AI_ACTION_TITLE[ai.action] : ''}
        open={!!ai}
        onCancel={() => setAi(undefined)}
        width={680}
        footer={[
          aiInsertable && ai?.text ? (
            <Button key="insert" type="primary" onClick={insertAiResult}>
              写回编辑器
            </Button>
          ) : null,
          ai?.text ? (
            <Button
              key="copy"
              onClick={() => {
                if (ai.text) void writeClipboardText(ai.text).then(() => message.success('已复制'))
              }}
            >
              复制
            </Button>
          ) : null,
          <Button key="close" onClick={() => setAi(undefined)}>
            关闭
          </Button>,
        ]}
      >
        {ai?.loading && (
          <div style={{ padding: '36px 0', textAlign: 'center' }}>
            <Spin />
            <div style={{ marginTop: 12, color: 'var(--text-secondary)', fontSize: 13 }}>
              AI 分析中…（通常需要 5~30 秒）
            </div>
          </div>
        )}
        {ai && !ai.loading && ai.error && (
          <Alert type="error" showIcon message="AI 请求失败" description={ai.error} />
        )}
        {ai && !ai.loading && ai.text && (
          <div style={{ maxHeight: '55vh', overflow: 'auto' }}>
            {ai.action === 'explain' ? (
              <div className="chat-md">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>{ai.text}</ReactMarkdown>
              </div>
            ) : (
              <pre
                style={{
                  margin: 0,
                  padding: '10px 12px',
                  background: 'var(--bg-inset)',
                  border: '1px solid var(--border-subtle)',
                  borderRadius: 6,
                  fontFamily: 'Consolas, Monaco, "Courier New", monospace',
                  fontSize: 12.5,
                  lineHeight: 1.6,
                  whiteSpace: 'pre-wrap',
                  color: 'var(--text-primary)',
                }}
              >
                {ai.text}
              </pre>
            )}
          </div>
        )}
      </Modal>
    </div>
  )
}
