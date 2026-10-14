import { useEffect, useRef, useState } from 'react'
import {
  deletePetPack,
  importPetPackViaDialog,
  isPackCapable,
  setEnabledPack,
  type PackImportError,
} from '../pack/packApi'
import type { PetPackInfo } from '../pack/packRuntime'

/** 删除二次确认：2.5s 未确认自动撤销（防误删，与对话历史删除同约定） */
const CONFIRM_MS = 2500

/**
 * 动作包管理面板：对齐 ChatPanel 覆盖式抽屉风格。
 * 列出内置形象 + 用户动作包，支持导入 zip / 启用 / 删除 / 恢复内置；
 * 浏览器宿主（!isPackCapable）降级为只读 + 提示。
 */
export function PetPackPanel({
  packs,
  activePackId,
  onChanged,
  onClose,
}: {
  packs: PetPackInfo[]
  /** 当前启用的用户包 id；null = 内置 */
  activePackId: string | null
  /** 导入/启用/删除成功后通知宿主刷新 runtime */
  onChanged: () => void
  onClose: () => void
}) {
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [confirmDel, setConfirmDel] = useState<string | null>(null)
  const [specOpen, setSpecOpen] = useState(false)
  const confirmTimer = useRef(0)

  useEffect(() => () => window.clearTimeout(confirmTimer.current), [])

  const requestDelete = (id: string) => {
    if (confirmDel === id) {
      window.clearTimeout(confirmTimer.current)
      setConfirmDel(null)
      setBusy(true)
      deletePetPack(id)
        .then(onChanged)
        .catch((e: string) => setError(e))
        .finally(() => setBusy(false))
      return
    }
    setConfirmDel(id)
    window.clearTimeout(confirmTimer.current)
    confirmTimer.current = window.setTimeout(() => setConfirmDel(null), CONFIRM_MS)
  }

  const enable = (id: string | null) => {
    setBusy(true)
    setError(null)
    setEnabledPack(id)
      .then(onChanged)
      .catch((e: string) => setError(e))
      .finally(() => setBusy(false))
  }

  const importPack = () => {
    setBusy(true)
    setError(null)
    importPetPackViaDialog()
      .then((info) => {
        if (info) onChanged()
      })
      .catch((e: unknown) => {
        setError(formatImportError(e))
      })
      .finally(() => setBusy(false))
  }

  return (
    <div className="chat-panel pack-panel">
      <div className="chat-head">
        <span>动作包管理</span>
        <button type="button" className="chat-close" onClick={onClose}>
          关闭
        </button>
      </div>

      <div className="pack-body">
        {!isPackCapable && (
          <div className="pack-hint">浏览器预览模式不支持自定义动作包，请使用桌面版。</div>
        )}
        {error && <div className="pack-error">{error}</div>}

        <div className="pack-list">
          {/* 内置形象：不可删除，随时可恢复 */}
          <div className={`pack-row${activePackId === null ? ' pack-row-active' : ''}`}>
            <div className="pack-info">
              <div className="pack-name">内置 · 鲸鱼娘</div>
              <div className="pack-meta">全部 10 个动作 · 默认形象</div>
            </div>
            {activePackId !== null && (
              <button
                type="button"
                className="pack-btn"
                disabled={busy || !isPackCapable}
                onClick={() => enable(null)}
              >
                恢复内置
              </button>
            )}
          </div>

          {packs.map((p) => (
            <div key={p.id} className={`pack-row${p.active ? ' pack-row-active' : ''}`}>
              <div className="pack-info">
                <div className="pack-name">{p.name}</div>
                <div className="pack-meta">
                  v{p.version}
                  {p.author ? ` · ${p.author}` : ''} · 覆盖 {p.states.length} 个动作
                </div>
              </div>
              <div className="pack-actions">
                {!p.active && (
                  <button
                    type="button"
                    className="pack-btn"
                    disabled={busy || !isPackCapable}
                    onClick={() => enable(p.id)}
                  >
                    启用
                  </button>
                )}
                <button
                  type="button"
                  className={`pack-btn pack-btn-danger${confirmDel === p.id ? ' pack-btn-armed' : ''}`}
                  disabled={busy || !isPackCapable}
                  onClick={() => requestDelete(p.id)}
                >
                  {confirmDel === p.id ? '确认删除？' : '删除'}
                </button>
              </div>
            </div>
          ))}

          {isPackCapable && packs.length === 0 && (
            <div className="pack-hint">还没有导入动作包。按规范做一个 zip，点下方「导入动作包」。</div>
          )}
        </div>

        <button
          type="button"
          className="pack-btn pack-import"
          disabled={busy || !isPackCapable}
          onClick={importPack}
        >
          {busy ? '处理中…' : '导入动作包（zip）'}
        </button>

        <button
          type="button"
          className="pack-spec-toggle"
          onClick={() => setSpecOpen((v) => !v)}
        >
          {specOpen ? '▾' : '▸'} 动作规范说明
        </button>
        {specOpen && (
          <div className="pack-spec">
            <p>zip 根目录放 pack.json 与各动作的素材目录（帧序列/单图/单视频）：</p>
            <pre>{`my-pack.zip
├── pack.json
├── idle_breath/idle_breath_00.png …   # 帧序列
├── wave/wave.webm                     # 透明视频
└── hello/hello.gif                    # 动图`}</pre>
            <p>素材放动作目录 &lt;key&gt;/ 下，src 写目录内文件名（实际路径 &lt;key&gt;/&lt;src&gt;）。</p>
            <p>
              pack.json 需含 schema（1/2）、id（[a-z0-9_-]{'{1,32}'}）、name、version；
              states 里每个动作声明素材形态——frames（帧序列）/ image（单图·动图）/
              video（webm/mp4，支持透明 webm）。可只提供部分动作，缺失动作自动回落内置动画。
            </p>
            <p>
              v2 可用 behavior 节自定义动作图：自己的动作名、触发器映射（何时播哪个动作）、
              定时转换规则与时间参数——不改代码即可改宠物行为。详见完整规范。
            </p>
            <a className="pack-link" href={`${import.meta.env.BASE_URL}pet-pack-spec.md`} download>
              下载完整动作规范 v2（pet-pack-spec.md）
            </a>
            <a className="pack-link" href={`${import.meta.env.BASE_URL}pet-pack-template.zip`} download>
              下载最小模板包（pet-pack-template.zip）
            </a>
          </div>
        )}
      </div>
    </div>
  )
}

/** Rust PackImportError / 其他异常 → 展示文本 */
function formatImportError(e: unknown): string {
  if (e && typeof e === 'object' && 'message' in e && 'code' in e) {
    const err = e as PackImportError
    return err.detail ? `${err.message}（${err.detail}）` : err.message
  }
  return e instanceof Error ? e.message : String(e)
}
