import type { PetView } from '../pet/usePet'

const EVENT_LABEL: Record<string, string> = {
  'ws-open': 'WS 连接',
  'ws-close': 'WS 断开',
  'session-opened': '会话开启',
  'session-closed': '会话关闭',
  output: '终端输出',
  'hostkey-unknown': '待确认指纹',
  'hostkey-confirmed': '指纹已确认',
  'agent-error': '错误→炸毛',
  'pointer-enter': '悬停→招手',
  poke: '戳',
  'drag-start': '拖起',
  'drag-end': '放下',
  blink: '眨眼',
  tick: 'tick',
}

/** ?debug 时显示：当前状态/上下文/事件流，用于对照真实 Agent 验证状态机 */
export function DebugPanel({ view }: { view: PetView }) {
  return (
    <div className="pet-debug">
      <div className="pet-debug-row">
        <b>{view.state}</b>
        <span>base={view.base}</span>
        <span>{view.connected ? '已连接' : '未连接'}</span>
        <span>sessions={view.sessions}</span>
      </div>
      <ul>
        {view.log
          .filter((e) => e.type !== 'tick')
          .slice(0, 10)
          .map((e, i) => (
            <li key={i}>{EVENT_LABEL[e.type] ?? e.type}</li>
          ))}
      </ul>
    </div>
  )
}
