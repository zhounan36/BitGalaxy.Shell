import { useMemo } from 'react'
import CodeMirror from '@uiw/react-codemirror'
import { EditorView, keymap } from '@codemirror/view'
import type { Extension } from '@codemirror/state'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'
import { sql } from '@codemirror/lang-sql'

/** SQL 编辑器（CodeMirror 6）：深色主题、行号、Ctrl+Enter 执行快捷键 */
export function SqlEditor({
  value,
  onChange,
  onRun,
  readOnly,
}: {
  value: string
  onChange: (v: string) => void
  /** Ctrl+Enter（Mac 上 Cmd+Enter）：编辑器自行取选区文本一并回调 */
  onRun: (text: string, hasSelection: boolean) => void
  readOnly?: boolean
}) {
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

  return (
    <CodeMirror
      value={value}
      onChange={onChange}
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
  )
}
