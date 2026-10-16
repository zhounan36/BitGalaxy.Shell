/**
 * 一键合包：前端 → 四 EXE（release）→ 产物汇总 → NSIS 安装包。
 * 用法：npm run desktop:pack
 * 可选环境变量：
 *   BGSH_NSIS   makensis.exe 路径（默认 %LOCALAPPDATA%\tauri\NSIS\makensis.exe，
 *               或 PATH 中的 makensis——tauri CLI 打过包会自动缓存前者）
 *   BGSH_SKIP_UI=1  跳过前端与资产步骤（只重编 Rust + 打包，调试安装脚本用）
 * 输出：dist/BitGalaxy-Shell_<version>_x64-setup.exe（组件可选安装/卸载）
 */
import { execSync, spawnSync } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')

// cargo 在 PATH（rustup 默认装到 ~/.cargo/bin；与 prep-drivers.mjs 同款处理）
if (process.platform === 'win32') {
  const cargoBin = join(process.env.USERPROFILE || '', '.cargo', 'bin')
  process.env.PATH = `${cargoBin};${process.env.PATH || ''}`
}

function run(cmd, opts = {}) {
  console.log(`\n$ ${cmd}`)
  execSync(cmd, { cwd: root, stdio: 'inherit', ...opts })
}

// ── 1. 前端 + 资产 + 驱动（可跳过，调试安装脚本时省时）────────────────────
if (process.env.BGSH_SKIP_UI !== '1') {
  run('npm run desktop:assets')
}

// ── 2. 四 EXE（根 workspace，共享 target 与依赖缓存）─────────────────────
run('cargo build --release -p bgshell-desktop -p bgshell-ssh -p bgshell-sql -p bgshell-serial')

// ── 3. 汇总产物 + WebView2 bootstrapper ──────────────────────────────────
run('node scripts/prep-pack.mjs')

// ── 4. makensis 编译安装包 ───────────────────────────────────────────────
function findMakensis() {
  if (process.env.BGSH_NSIS && existsSync(process.env.BGSH_NSIS)) return process.env.BGSH_NSIS
  const cached = join(
    process.env.LOCALAPPDATA || '', 'tauri', 'NSIS', 'makensis.exe',
  )
  if (existsSync(cached)) return cached
  // PATH 里的 makensis（用户自装 NSIS）
  try {
    const where = execSync('where makensis', { encoding: 'utf8' }).split('\r\n')[0]
    if (where && existsSync(where.trim())) return where.trim()
  } catch { /* PATH 无 */ }
  return null
}

const makensis = findMakensis()
if (!makensis) {
  console.error(
    '找不到 makensis.exe：请设置 BGSH_NSIS 指向 NSIS 3.x 安装位置，' +
      '或运行一次 tauri 打包让 CLI 自动缓存（%LOCALAPPDATA%\\tauri\\NSIS）。',
  )
  process.exit(1)
}

const version = JSON.parse(
  readFileSync(join(root, 'apps', 'desktop', 'src-tauri', 'tauri.conf.json'), 'utf8'),
).version
const outfile = join('dist', `BitGalaxy-Shell_${version}_x64-setup.exe`)
mkdirSync(join(root, 'dist'), { recursive: true })

// NSIS Unicode 模式要求脚本带 UTF-8 BOM（中文组件名/描述）；
// 源文件被编辑器改为无 BOM 时此处自动补上（幂等）
const nsi = join(root, 'scripts', 'installer.nsi')
const BOM = Buffer.from([0xef, 0xbb, 0xbf])
const head = readFileSync(nsi).subarray(0, 3)
if (!head.equals(BOM)) {
  const body = readFileSync(nsi)
  writeFileSync(nsi, Buffer.concat([BOM, body]))
  console.log('installer.nsi 缺 UTF-8 BOM，已自动补上')
}

console.log(`\n$ makensis（版本 ${version}）`)
const r = spawnSync(
  makensis,
  [
    '-NOCD', // 脚本内相对路径基于 cwd（target/pack 用根相对路径）
    `-DVERSION=${version}`,
    '-DPACKDIR=target\\pack',
    `-DOUTFILE=${outfile}`,
    'scripts/installer.nsi',
  ],
  { cwd: root, stdio: 'inherit' },
)
if (r.status !== 0) process.exit(r.status || 1)

console.log(`\n安装包就绪: ${join(root, outfile)}`)
