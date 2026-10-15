import { useEffect, useMemo } from 'react'
import { Alert, Button, Modal, Spin, message } from 'antd'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { writeClipboardText } from '../api/clipboard'

/** 一次性 SQL AI 问答的 system：SQL 专家角色（主程序 AiHandler 直接透传给 LLM） */
export const SQL_AI_SYSTEM =
  '你是资深数据库专家，精通 SQL 与主流数据库方言（MySQL/PostgreSQL/SQLite 等）。回答使用中文，准确、简洁。'

/** 无语言标注代码块的内容需像 SQL 才会被提取（避免把 bash/json 误当 SQL） */
const SQL_HEAD_RE =
  /^(select|with|insert\s+into|replace\s+into|update|delete\s+from|create\s+(table|index|view|database|temporary)|alter|drop|truncate|rename|explain|pragma|show)\b/i

/** 从 AI 回复中提取 SQL 代码块：```sql 标注的块，或无语言标注但内容像 SQL 的块 */
export function extractSqlBlocks(text: string): string[] {
  const blocks: string[] = []
  const re = /```([a-zA-Z]*)[^\S\n]*\n([\s\S]*?)```/g
  for (let m = re.exec(text); m; m = re.exec(text)) {
    const lang = m[1].toLowerCase()
    const code = m[2].trim()
    if (code && (lang === 'sql' || (lang === '' && SQL_HEAD_RE.test(code)))) blocks.push(code)
  }
  return blocks
}

/** SQL 面板通用 AI 结果弹窗：加载/错误/展示 + 智能提取 SQL 代码块 + 写回编辑器 / 复制 /
 *  新结果自动复制到剪贴板。markdown=true 用于解释/问答类回复（提取到的 SQL 代码块
 *  才提供「写回编辑器 / 复制 SQL」并自动复制）；false 用于优化/注释类纯 SQL 产出。 */
export function SqlAiModal({
  open,
  title,
  markdown,
  loading,
  error,
  text,
  onInsert,
  onClose,
  autoCopy,
}: {
  open: boolean
  title: string
  /** 解释/问答类回复用 Markdown 渲染；优化/注释类展示纯 SQL */
  markdown?: boolean
  loading?: boolean
  error?: string
  text?: string
  /** 提供「写回编辑器」回调（未提供则不显示该按钮） */
  onInsert?: (sql: string) => void
  onClose: () => void
  /** 新结果到达时自动把产出的 SQL 复制到系统剪贴板（可直接 Ctrl+V 粘贴） */
  autoCopy?: boolean
}) {
  const blocks = useMemo(() => (markdown && text ? extractSqlBlocks(text) : []), [markdown, text])
  /** 可写回/复制的 SQL：纯 SQL 动作用全文；Markdown 回复用提取出的代码块 */
  const sqlText = markdown ? blocks.join('\n\n') : text?.trim()

  // 新结果到达且 autoCopy：自动进剪贴板（Markdown 回复仅在提取到 SQL 时复制）
  useEffect(() => {
    if (!open || loading || !sqlText || !autoCopy) return
    void writeClipboardText(sqlText).then(
      () => message.success('AI 的 SQL 已自动复制到剪贴板'),
      () => {},
    )
  }, [open, loading, sqlText, autoCopy])

  const footer = [
    onInsert && sqlText ? (
      <Button key="insert" type="primary" onClick={() => onInsert(sqlText)}>
        写回编辑器
      </Button>
    ) : null,
    sqlText ? (
      <Button
        key="copy"
        onClick={() => {
          void writeClipboardText(sqlText).then(() => message.success('已复制'))
        }}
      >
        {markdown ? '复制 SQL' : '复制'}
      </Button>
    ) : null,
    <Button key="close" onClick={onClose}>
      关闭
    </Button>,
  ]

  return (
    <Modal title={title} open={open} onCancel={onClose} width={680} footer={footer}>
      {loading && (
        <div style={{ padding: '36px 0', textAlign: 'center' }}>
          <Spin />
          <div style={{ marginTop: 12, color: 'var(--text-secondary)', fontSize: 13 }}>
            AI 分析中…（通常需要 5~30 秒）
          </div>
        </div>
      )}
      {!loading && error && <Alert type="error" showIcon message="AI 请求失败" description={error} />}
      {!loading && text && (
        <div style={{ maxHeight: '55vh', overflow: 'auto' }}>
          {markdown ? (
            <div className="chat-md">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown>
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
              {text}
            </pre>
          )}
        </div>
      )}
    </Modal>
  )
}
