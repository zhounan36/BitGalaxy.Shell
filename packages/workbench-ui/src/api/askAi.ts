import { aiSocket } from './ws'

/** askAi 结果（ai-ask-result 帧） */
export interface AskAiOutcome {
  ok: boolean
  text: string
  message?: string
}

/** 一次性 AI 问答（ai op:"ask"）：无对话历史、无工具、不落库，reqId 关联回包。
 *  AI 服务由主程序提供（AiHandler 在 7777 主程序进程）；主程序未运行时给出
 *  可读错误。用于 SQL 终端右键「解释/优化/注释」等轻量场景。 */
export async function askAi(text: string, system?: string, timeoutMs = 150000): Promise<AskAiOutcome> {
  try {
    // 3 次重试（约 1.5s）：主程序未运行时快速失败，不长时间等待
    await aiSocket.connect(3)
  } catch {
    return {
      ok: false,
      text: '',
      message: '无法连接 AI 服务：请先启动主程序 BitGalaxy Shell（AI 由主程序提供）',
    }
  }
  const reqId = crypto.randomUUID()
  return new Promise<AskAiOutcome>((resolve) => {
    let settled = false
    const finish = (r: AskAiOutcome) => {
      if (settled) return
      settled = true
      window.clearTimeout(timer)
      offResult()
      offState()
      resolve(r)
    }
    const offResult = aiSocket.onControl((frame) => {
      if (frame.t === 'ai-ask-result' && frame.m.reqId === reqId) {
        finish({ ok: frame.m.ok, text: frame.m.text, message: frame.m.message })
      }
    })
    // 断线即失败（等不到回包时不必傻等超时）
    const offState = aiSocket.onControl((frame) => {
      if (frame.t === 'error' && frame.m.code === 'disconnected') {
        finish({ ok: false, text: '', message: '与 AI 服务的连接已断开，请重试' })
      }
    })
    const timer = window.setTimeout(() => {
      finish({ ok: false, text: '', message: 'AI 请求超时，请重试' })
    }, timeoutMs)
    aiSocket.sendControl({ t: 'ai', m: { op: 'ask', reqId, text, system } })
  })
}
