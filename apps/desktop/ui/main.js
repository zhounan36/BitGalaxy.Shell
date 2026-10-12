// 壳 loading 页：等待 sidecar（bgshell-agent）就绪后跳转到工作台（Agent 内嵌 UI，单一直实来源）
const AGENT = 'http://127.0.0.1:7777'
const msg = document.getElementById('msg')
let tries = 0

async function poll() {
  try {
    // no-cors：成功即 opaque 响应，仅用于探活
    await fetch(`${AGENT}/`, { mode: 'no-cors', cache: 'no-store' })
    location.replace(`${AGENT}/`)
    return
  } catch {
    // 未就绪，继续等待
  }
  tries += 1
  if (tries > 120) {
    msg.textContent = 'Agent 启动超时：请查看 %APPDATA%\\bitgalaxy-shell\\agent.log'
    return
  }
  msg.textContent = `正在启动本机 Agent …（${tries}）`
  setTimeout(poll, 500)
}

poll()
