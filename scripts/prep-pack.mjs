/**
 * NSIS 合包预备：把四 EXE + SQL 驱动 + PET 资源 + WebView2 bootstrapper
 * 汇总到 target/pack/，供 scripts/installer.nsi 打包（详见同目录 NSIS 脚本）。
 * 布局与运行期解析约定一致：所有 EXE 平铺（spawn_sibling_exe 按 exe 同级找），
 * SQL 驱动在 drivers/{name}/host.exe（DriverManager::bundled），
 * PET 资源在 resources/pet-packs/（builtin_pack_root，缺目录降级无精灵）。
 */
import { execSync } from 'node:child_process'
import { cpSync, existsSync, mkdirSync, readdirSync, statSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const releaseDir = join(root, 'target', 'release')
const packDir = join(root, 'target', 'pack')
const wvCacheDir = join(root, 'target', 'webview2')

// 四 EXE（根 workspace 统一 target，crate 名即产物名）
const EXES = ['bgshell-desktop.exe', 'bgshell-ssh.exe', 'bgshell-sql.exe', 'bgshell-serial.exe']
// SQL 驱动源（prep-drivers.mjs 的拷贝产物）
const driversSrc = join(root, 'apps', 'sql', 'src-tauri', 'drivers')
// PET 内置动作包源（src-tauri/resources 下非 gitignore，src 即产物）
const petSrc = join(root, 'apps', 'desktop', 'src-tauri', 'resources', 'pet-packs')
// WebView2 bootstrapper（微软官方永久固定链接，下载一次缓存到 target/webview2/）
const WV_URL = 'https://go.microsoft.com/fwlink/p/?LinkId=2124703'

function fail(msg) {
  console.error(msg)
  process.exit(1)
}

function copyIfExists(name, src, dest) {
  if (!existsSync(src)) fail(`缺少构建产物：${src}（请先完成对应构建步骤）`)
  cpSync(src, dest, { recursive: true })
  console.log(`  + ${name}`)
}

// ── 1. 四 EXE ────────────────────────────────────────────────────────────
console.log('[prep-pack] 汇总四 EXE →', packDir)
mkdirSync(packDir, { recursive: true })
for (const exe of EXES) {
  const src = join(releaseDir, exe)
  if (!existsSync(src)) fail(`缺少 ${src}（请先 cargo build --release）`)
  cpSync(src, join(packDir, exe))
  console.log(`  + ${exe}  (${(statSync(src).size / 1048576).toFixed(1)} MB)`)
}

// ── 2. SQL 驱动（SQL 组件）──────────────────────────────────────────────
console.log('[prep-pack] SQL 驱动宿主')
copyIfExists('drivers/', driversSrc, join(packDir, 'drivers'))

// ── 3. PET 资源（PET 组件，体积大头）────────────────────────────────────
console.log('[prep-pack] PET 内置动作包')
copyIfExists('resources/pet-packs/', petSrc, join(packDir, 'resources', 'pet-packs'))

// ── 4. WebView2 bootstrapper ────────────────────────────────────────────
const wvFile = join(wvCacheDir, 'MicrosoftEdgeWebview2Setup.exe')
if (!existsSync(wvFile)) {
  console.log('[prep-pack] 下载 WebView2 bootstrapper（一次，缓存 target/webview2/）……')
  mkdirSync(wvCacheDir, { recursive: true })
  try {
    // Windows：curl 系统自带；跟随重定向到 msedge 官方 CDN
    execSync(`curl -L --fail -o "${wvFile}" "${WV_URL}"`, { cwd: root, stdio: 'inherit' })
  } catch {
    fail(
      `下载失败：${WV_URL}\n` +
        `离线环境请手动下载放置到 ${wvFile} 后重跑本脚本。`,
    )
  }
} else {
  console.log('[prep-pack] WebView2 bootstrapper 已缓存')
}
cpSync(wvFile, join(packDir, 'MicrosoftEdgeWebview2Setup.exe'))

// ── 5. 汇总报告 ─────────────────────────────────────────────────────────
const total = readdirSync(packDir, { recursive: true })
  .filter((f) => statSync(join(packDir, String(f))).isFile())
  .reduce((s, f) => s + statSync(join(packDir, String(f))).size, 0)
console.log(`[prep-pack] 就绪：${packDir}（共 ${(total / 1048576).toFixed(1)} MB）`)
