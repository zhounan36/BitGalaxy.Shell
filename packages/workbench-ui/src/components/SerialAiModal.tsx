import { useEffect, useMemo } from 'react'
import { Alert, Button, Modal, Spin, message } from 'antd'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { writeClipboardText } from '../api/clipboard'

/** 串口 AI 一次性问答的 system：串口通信/嵌入式协议专家（主程序 AiHandler 直接透传给 LLM） */
export const SERIAL_AI_SYSTEM =
  '你是资深嵌入式与串口通信专家，精通常见串口协议与帧结构分析（帧头/长度域/命令字/数据域/校验，' +
  '如异或校验/累加和/CRC8/CRC16-Modbus，以及 ASCII 行协议、Modbus RTU/ASCII、AT 指令、NMEA 等）。' +
  '回答使用中文，准确、简洁。分析数据帧时逐字节标注偏移与含义，并指出疑似校验算法与计算范围；' +
  '生成发送指令时只输出指令本身，不要任何解释。'

/** 从 AI 回复提取的可发送内容（hex 开关随块携带，填入发送框时联动格式切换） */
export interface SerialAiBlock {
  content: string
  hex: boolean
}

/** 无语言标注代码块需像 hex 字节串才判定为 hex（避免把普通文本误判为 hex） */
function looksLikeHex(code: string): boolean {
  const clean = code.replace(/\s+/g, '')
  return clean.length > 0 && clean.length % 2 === 0 && /^[0-9a-fA-F]+$/.test(clean)
}

/** 从 AI 回复中提取可发送指令代码块：```hex 标注的块，```text 的文本块，
 *  或无语言标注但内容像 hex 字节串的块 */
export function extractSerialBlocks(text: string): SerialAiBlock[] {
  const blocks: SerialAiBlock[] = []
  const re = /```([a-zA-Z]*)[^\S\n]*\n([\s\S]*?)```/g
  for (let m = re.exec(text); m; m = re.exec(text)) {
    const lang = m[1].toLowerCase()
    const code = m[2].trim()
    if (!code) continue
    if (lang === 'hex' || (lang === '' && looksLikeHex(code))) blocks.push({ content: code, hex: true })
    else if (lang === 'text') blocks.push({ content: code, hex: false })
  }
  return blocks
}

/** 串口面板通用 AI 结果弹窗：加载/错误/Markdown 展示 + 智能提取指令代码块 +
 *  填入发送框 / 复制 / 新结果自动复制到剪贴板（用于「AI 解析数据帧」与
 *  「AI 生成指令」两个动作；解析类回复不提供 onInsert，仅展示）。 */
export function SerialAiModal({
  open,
  title,
  loading,
  error,
  text,
  insertLabel = '填入发送框',
  onInsert,
  onClose,
  autoCopy,
}: {
  open: boolean
  title: string
  loading?: boolean
  error?: string
  text?: string
  /** onInsert 存在且提取到代码块时显示的插入按钮文案 */
  insertLabel?: string
  /** 提供时展示「填入发送框」按钮（携带 hex 标志） */
  onInsert?: (b: SerialAiBlock) => void
  onClose: () => void
  /** 新结果到达时自动把产出的指令复制到系统剪贴板 */
  autoCopy?: boolean
}) {
  const blocks = useMemo(() => (text ? extractSerialBlocks(text) : []), [text])
  const insertable = onInsert && blocks.length > 0 ? blocks[0] : undefined

  // 新结果到达且 autoCopy：自动进剪贴板（可直接 Ctrl+V 粘贴）
  useEffect(() => {
    if (!open || loading || !insertable || !autoCopy) return
    void writeClipboardText(insertable.content).then(
      () => message.success('AI 的指令已自动复制到剪贴板'),
      () => {},
    )
  }, [open, loading, insertable, autoCopy])

  const footer = [
    insertable ? (
      <Button key="insert" type="primary" onClick={() => onInsert!(insertable)}>
        {insertLabel}
      </Button>
    ) : null,
    insertable ? (
      <Button
        key="copy"
        onClick={() => {
          void writeClipboardText(insertable.content).then(() => message.success('已复制'))
        }}
      >
        复制指令
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
          <div className="chat-md">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown>
          </div>
        </div>
      )}
    </Modal>
  )
}
