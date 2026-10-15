import { useMemo, useRef, useState } from 'react'
import CodeMirror from '@uiw/react-codemirror'
import { EditorView, keymap } from '@codemirror/view'
import type { Extension } from '@codemirror/state'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'
import { sql } from '@codemirror/lang-sql'
import { Input, Menu, Modal, message } from 'antd'
import type { MenuProps } from 'antd'
import {
  BulbOutlined,
  ClearOutlined,
  CommentOutlined,
  CopyOutlined,
  PlayCircleOutlined,
  QuestionCircleOutlined,
  RocketOutlined,
  SelectOutlined,
} from '@ant-design/icons'
import type { EditorView as CMEditorView } from '@codemirror/view'
import { askAi } from '../api/askAi'
import { writeClipboardText } from '../api/clipboard'
import { SQL_AI_SYSTEM, SqlAiModal } from './SqlAiModal'

type AiAction = 'explain' | 'optimize' | 'comment' | 'ask'

const AI_ACTION_TITLE: Record<AiAction, string> = {
  explain: 'AI 解释 SQL',
  optimize: 'AI 优化 SQL',
  comment: 'AI 注释 SQL',
  ask: '问 AI',
}

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
  question?: string
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
  const [askOpen, setAskOpen] = useState(false)
  const [question, setQuestion] = useState('')

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

  /** AI 动作提示词（方言上下文 + 只输出要求；ask 附带自定义问题） */
  const aiPrompt = (action: AiAction, sql: string, question = '') => {
    const dialect = dbType ? `数据库类型：${dbType}。` : ''
    if (action === 'explain') {
      return `请解释下面这段 SQL：说明它做了什么、关键子句的作用，以及可能的问题或风险。${dialect}\n\n${sql}`
    }
    if (action === 'optimize') {
      return `请优化下面这段 SQL，提升可读性与执行效率（仅在明显有益时补充条件或索引提示，不得改变查询语义）。只输出优化后的 SQL 本身，不要任何解释。${dialect}\n\n${sql}`
    }
    if (action === 'comment') {
      return `请为下面这段 SQL 的关键行添加简洁的中文行内注释（-- 注释）。只输出带注释的 SQL 本身，不要任何解释。${dialect}\n\n${sql}`
    }
    return `请针对下面这段 SQL 回答问题：「${question}」。回答使用中文，准确、简洁；如回答中包含 SQL，请务必用 \`\`\`sql 代码块给出完整可执行语句。${dialect}\n\n${sql}`
  }

  const startAi = (action: AiAction, question = '') => {
    const s = selectionOrAll()
    if (!s.text.trim()) return
    setAi({ action, sql: s.text, hadSelection: s.hasSelection, question, loading: true })
    void askAi(aiPrompt(action, s.text, question), SQL_AI_SYSTEM).then((r) => {
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

  /** 发送自定义问题（Enter 发送，Shift+Enter 换行） */
  const sendAsk = () => {
    const q = question.trim()
    if (!q) return
    setAskOpen(false)
    startAi('ask', q)
  }

  /** 把 AI 产出写回编辑器：替换当前选区（发起时通常选中了 SQL），无选区则插入光标处 */
  const insertAiResult = (text: string) => {
    const view = viewRef.current
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
      {
        key: 'ai-ask',
        label: '问 AI（附此 SQL）',
        icon: <QuestionCircleOutlined />,
        disabled: !menu.hasText,
      },
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
    if (key === 'ai-ask') setAskOpen(true)
  }

  /** AI 结果是否适合写回编辑器（优化/注释/问答产出 SQL；解释是说明文本） */
  const aiInsertable = ai && ai.action !== 'explain'

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

      {/* ── 自定义问 AI：输入问题，自动附带选区/全文 SQL 作为上下文 ── */}
      <Modal
        title="问 AI"
        open={askOpen}
        onCancel={() => setAskOpen(false)}
        onOk={sendAsk}
        okText="发送"
        cancelText="取消"
        okButtonProps={{ disabled: !question.trim() }}
        width={560}
      >
        <Input.TextArea
          autoFocus
          value={question}
          onChange={(e) => setQuestion(e.target.value)}
          placeholder="例如：改成联表查询 / 加上分页 / 这个查询为什么慢？"
          rows={3}
          onPressEnter={(e) => {
            if (!e.shiftKey) {
              e.preventDefault()
              sendAsk()
            }
          }}
        />
        {(() => {
          const s = selectionOrAll()
          return s.text.trim() ? (
            <div style={{ marginTop: 8, fontSize: 12, color: 'var(--text-secondary)' }}>
              将自动附带{s.hasSelection ? '选中' : '全部'}的 SQL（{s.text.length} 字符）作为上下文；AI 产出的 SQL 可一键写回或已自动复制
            </div>
          ) : null
        })()}
      </Modal>

      {/* ── AI 结果弹窗（共享组件）：加载/错误/展示 + 提取 SQL 写回/复制/自动进剪贴板 ── */}
      <SqlAiModal
        open={!!ai}
        title={
          ai?.action === 'ask' && ai.question
            ? `问 AI：${ai.question.length > 30 ? `${ai.question.slice(0, 30)}…` : ai.question}`
            : ai
              ? AI_ACTION_TITLE[ai.action]
              : ''
        }
        markdown={ai?.action === 'explain' || ai?.action === 'ask'}
        loading={ai?.loading}
        error={ai?.error}
        text={ai?.text}
        autoCopy={ai?.action !== 'explain'}
        onInsert={aiInsertable ? insertAiResult : undefined}
        onClose={() => setAi(undefined)}
      />
    </div>
  )
}
