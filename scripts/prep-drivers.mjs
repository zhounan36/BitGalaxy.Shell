/**
 * SQL 驱动宿主预备：在根 Cargo workspace 构建三个驱动宿主（release）并把 exe 拷到
 * apps/sql/src-tauri/drivers/{name}/host(.exe)（SQL 独立应用随包内置；P6 起主程序
 * 基座不再携带 SQL 能力，不再拷 desktop）。
 * tauri.conf.json 的 bundle.resources 驱动 glob 会把它们内置进安装包（安装后位于
 * 安装目录 drivers/{name}/host.exe），运行期 DriverManager::bundled 按
 * 「exe 同级 drivers/{name}/host(.exe)」解析；开发期 tauri-build 同样会
 * 拷到 target 目录，dev 模式与安装包行为一致。
 */
import { execSync } from 'node:child_process'
import { cpSync, rmSync, mkdirSync, existsSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const outDir = join(root, 'target', 'release')
// 注意：mjs 块注释内不得出现 glob 星号加斜杠序列（会提前终止注释）
const destRoots = [join(root, 'apps', 'sql', 'src-tauri', 'drivers')]
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

console.log('构建 SQL 驱动宿主（release，根 workspace）……')
execSync('cargo build --release -p host-mysql -p host-postgres -p host-sqlite', { cwd: root, stdio: 'inherit' })

for (const destRoot of destRoots) {
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
  }
  console.log(`SQL 驱动内置资产就绪: ${destRoot}`)
}
