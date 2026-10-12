import { createReadStream, existsSync, statSync } from 'node:fs'
import type { IncomingMessage, ServerResponse } from 'node:http'
import { extname, join, normalize, sep } from 'node:path'

const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.map': 'application/json',
}

export function lookupMime(pathname: string): string {
  return MIME[extname(pathname).toLowerCase()] ?? 'application/octet-stream'
}

/**
 * 静态工作台服务：assets 优先（SEA 单文件 exe 内嵌资源，内存 Map），
 * 否则读文件系统；SPA 回退 index.html；两种来源皆未就绪时返回构建提示。
 */
export function serveStatic(
  root: string,
  req: IncomingMessage,
  res: ServerResponse,
  assets?: Map<string, Buffer>,
): void {
  const url = new URL(req.url ?? '/', 'http://127.0.0.1')
  const pathname = decodeURIComponent(url.pathname)

  if (assets) {
    let key = pathname
    if (key.endsWith('/')) key += 'index.html'
    const buf = assets.get(key) ?? assets.get('/index.html')
    if (!buf) {
      res.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' })
      res.end('Not Found')
      return
    }
    res.writeHead(200, { 'content-type': lookupMime(key), 'cache-control': 'no-cache' })
    res.end(buf)
    return
  }

  let filePath = normalize(join(root, pathname))

  // 防目录穿越
  if (filePath !== root && !filePath.startsWith(root + sep)) {
    res.writeHead(403).end()
    return
  }

  if (!existsSync(filePath) || statSync(filePath).isDirectory()) {
    const index = join(root, 'index.html')
    if (!existsSync(index)) {
      res.writeHead(200, { 'content-type': 'text/plain; charset=utf-8' })
      res.end('workbench-ui 尚未构建：请在仓库根目录执行 npm run build:ui 后重启 agent')
      return
    }
    filePath = index
  }

  res.writeHead(200, { 'content-type': lookupMime(filePath), 'cache-control': 'no-cache' })
  createReadStream(filePath).pipe(res)
}
