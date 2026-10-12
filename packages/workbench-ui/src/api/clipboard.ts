/** 系统剪贴板读写（双宿主）：
 *  Tauri 壳内走原生插件 IPC（WebView2 下 navigator.clipboard.readText 不可靠）；
 *  浏览器宿主（云端工作台）回退 Web Clipboard API。 */
export async function readClipboardText(): Promise<string> {
  if ('__TAURI_INTERNALS__' in window) {
    const { invoke } = await import('@tauri-apps/api/core')
    return (await invoke<string | null>('plugin:clipboard-manager|read_text')) ?? ''
  }
  return await navigator.clipboard.readText()
}

export async function writeClipboardText(text: string): Promise<void> {
  if ('__TAURI_INTERNALS__' in window) {
    const { invoke } = await import('@tauri-apps/api/core')
    // write_text 必填参数是 text；label 仅为可选标签，此前误传导致调用被静默拒绝
    await invoke('plugin:clipboard-manager|write_text', { text })
    return
  }
  await navigator.clipboard.writeText(text)
}
