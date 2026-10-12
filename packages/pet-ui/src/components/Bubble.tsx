import { useEffect, useState } from 'react'
import type { PetView } from '../pet/usePet'
import { pickLine } from '../pet/lines'

/**
 * 台词/输出气泡：底部用生成的深海蓝蕾丝气泡底图做 9-slice（border-image）。
 * working 态显示终端输出尾部，其余态显示状态台词。
 */
export function Bubble({ view }: { view: PetView }) {
  const [line, setLine] = useState<string | null>(null)

  // 状态切换时重新抽一条台词（working 的输出尾部实时展示，不参与抽取）
  useEffect(() => {
    if (view.state === 'working') {
      setLine(null)
      return
    }
    setLine(pickLine(view.state))
    if (view.state === 'need_input' && view.pendingHostkey) setLine(null)
  }, [view.state])

  const content =
    view.state === 'working'
      ? view.outputTail.trim().split('\n').slice(-2).join('\n') || '让我想想……（内心独白中）'
      : view.state === 'need_input' && view.pendingHostkey
        ? `要确认指纹：${view.pendingHostkey.slice(0, 24)}…（请到工作台确认）`
        : line
  if (!content) return null

  return (
    <div className="pet-bubble" role="status">
      <pre>{content}</pre>
    </div>
  )
}
