import { useCallback, useEffect, useRef, useState } from 'react'
import { DataFrameType, type ServerFrame } from '@bgshell/protocol'
import { agentSocket } from '../api/ws'
import { PetMachine, type BehaviorSpec, type PetEvent, type PetSnapshot } from './machine'

/** 去掉 ANSI 转义与控制字符，仅留可读文本 */
function stripAnsi(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, '').replace(/[\x00-\x08\x0b-\x1f\x7f]/g, '')
}

/** 最近终端输出尾部（气泡预览用） */
const TAIL_MAX = 160

export interface PetView extends PetSnapshot {
  /** 终端输出尾部（working 气泡展示） */
  outputTail: string
}

/**
 * 把 Agent 事件流/交互事件/计时器喂给 PetMachine，并按帧刷新视图。
 * 返回 { view, dispatch }：dispatch 供组件上报交互事件（hover/拖拽/戳）。
 */
export function usePet(behavior?: BehaviorSpec | null): { view: PetView; dispatch: (ev: PetEvent) => void } {
  const machineRef = useRef<PetMachine>(null as unknown as PetMachine)
  if (!machineRef.current) machineRef.current = new PetMachine(behavior ?? undefined)
  // 换动作包时热切换行为表（自定义动作图）
  useEffect(() => {
    machineRef.current.applyBehavior(behavior ?? undefined)
  }, [behavior])
  const tailRef = useRef('')
  const nextBlinkAtRef = useRef(Date.now() + 6000 + Math.random() * 6000)
  const [view, setView] = useState<PetView>(() => ({
    ...machineRef.current.snapshot(),
    outputTail: '',
  }))

  const publish = useCallback(() => {
    setView({ ...machineRef.current.snapshot(), outputTail: tailRef.current })
  }, [])

  const dispatch = useCallback(
    (ev: PetEvent) => {
      machineRef.current.handle(ev)
      publish()
    },
    [publish],
  )

  useEffect(() => {
    const m = machineRef.current

    const onControl = (frame: ServerFrame) => {
      switch (frame.t) {
        case 'session-opened':
          // 指纹确认后 Agent 重发 session-opened：顺带清掉待确认态
          m.handle({ type: 'hostkey-confirmed' })
          m.handle({ type: 'session-opened' })
          break
        case 'session-closed':
          m.handle({ type: 'session-closed' })
          break
        case 'hostkey-unknown':
          m.handle({ type: 'hostkey-unknown', fingerprint: frame.m.fingerprint })
          break
        case 'error':
          if (frame.m.code === 'disconnected') m.handle({ type: 'ws-close' })
          else if (frame.m.code === 'open-failed') m.handle({ type: 'agent-error' })
          break
        case 'probe-result':
          if (!frame.m.ok) m.handle({ type: 'agent-error' })
          break
        default:
          break
      }
      publish()
    }

    const onData = (sid: string, type: number, payload: Uint8Array) => {
      if (type !== DataFrameType.Stdout && type !== DataFrameType.Stderr) return
      const text = stripAnsi(new TextDecoder().decode(payload))
      if (!text) return
      tailRef.current = (tailRef.current + text).slice(-TAIL_MAX)
      m.handle({ type: 'output' })
      publish()
    }

    const offControl = agentSocket.onControl(onControl)
    const offData = agentSocket.onData(onData)

    agentSocket
      .connect()
      .then(() => {
        m.handle({ type: 'ws-open' })
        publish()
      })
      .catch(() => {
        /* ws-close 由 onclose 触发；重连由 socket 自身负责 */
      })

    const tickTimer = window.setInterval(() => {
      const now = Date.now()
      // 待机眨眼调度（状态机按 natural.idle 门控 + 优先级丢弃，非待机态自动忽略）
      if (now >= nextBlinkAtRef.current) {
        m.handle({ type: 'blink' })
        nextBlinkAtRef.current = now + 6000 + Math.random() * 6000
      }
      m.handle({ type: 'tick', now })
      publish()
    }, 300)

    return () => {
      offControl()
      offData()
      window.clearInterval(tickTimer)
    }
  }, [publish])

  return { view, dispatch }
}
