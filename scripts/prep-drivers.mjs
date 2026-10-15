/**
 * SQL 驱动宿主预备：构建 drivers workspace（release）并把三个宿主 exe 拷到
 * apps/desktop/src-tauri/drivers/{name}/host(.exe)。tauri.conf.json 的
 * bundle.resources 驱动 glob 会把它们内置进安装包（安装后位于安装目录
 * drivers/{name}/host.exe），运行期 DriverManager::bundled 按
 * 「exe 同级 drivers/{name}/host(.exe)」解析；开发期 tauri-build 同样会
 * 拷到 target 目录，dev 模式与安装包行为一致。
 */
import { execSync } from 'node:child_process'
import { cpSync, rmSync, mkdirSync, existsSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const driversDir = join(root, 'drivers')
const outDir = join(driversDir, 'target', 'release')
const destRoot = join(root, 'apps', 'desktop', 'src-tauri', 'drivers')
const ext = process.platform === 'win32' ? '.exe' : ''

const DRIVERS = [
  { name: 'mysql', bin: `mysql-host${ext}` },
  { name: 'postgres', bin: `postgres-host${ext}` },
  { name: 'sqlite', bin: `sqlite-host${ext}` },
]

// cargo 在 PATH（rustup 默认装到 ~/.cargo/bin；与桌面构建脚本同款处理）
if (process.platform === 'win32') {
  const cargoBin = join(process.env.USERPROFILE || '', '.cargo', 'bin')
  process.env.PATH = `${cargoBin};${process.env.PATH || ''}`
}

console.log('构建 SQL 驱动宿主（release）……')
execSync('cargo build --release', { cwd: driversDir, stdio: 'inherit' })

// 整目录重建：驱动增删时自动清掉上一版残留
rmSync(destRoot, { recursive: true, force: true })
for (const { name, bin } of DRIVERS) {
  const src = join(outDir, bin)
  if (!existsSync(src)) {
    console.error(`缺少 ${src}，请检查 drivers 构建产物`)
    process.exit(1)
  }
  const dest = join(destRoot, name)
  mkdirSync(dest, { recursive: true })
  cpSync(src, join(dest, `host${ext}`))
  console.log(`驱动就绪: src-tauri/drivers/${name}/host${ext}`)
}
console.log(`SQL 驱动内置资产就绪: ${destRoot}`)
