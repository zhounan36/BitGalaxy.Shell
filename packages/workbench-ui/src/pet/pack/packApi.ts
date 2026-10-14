/**
 * 动作包 IPC 客户端（Tauri 宿主）：导入 / 列表 / 删除 / 启用。
 * 浏览器宿主无磁盘能力，所有函数由 UI 层降级（提示「仅桌面版可自定义」）。
 */
import { isTauri } from '../api/tauri'
import type { PetPackInfo } from './packRuntime'

export const isPackCapable = isTauri

/** 导入失败：Rust PackImportError 序列化结构 */
export interface PackImportError {
  code: string
  message: string
  detail?: string | null
}

/**
 * 拿到「动作包落盘目录 → 可加载 URL」转换函数（Tauri asset protocol，
 * Windows 下为 http://asset.localhost/...）。必须 await 后同步使用：
 * 帧 URL 在渲染热路径上生成，不适合 async。
 */
export async function createAssetUrlFactory(): Promise<(dir: string, file: string) => string> {
  const { convertFileSrc } = await import('@tauri-apps/api/core')
  return (dir, file) => convertFileSrc(`${dir}/${file}`.replace(/\\/g, '/'))
}

/** 列出全部用户动作包（含 active 标记） */
export async function listPetPacks(): Promise<PetPackInfo[]> {
  const { invoke } = await import('@tauri-apps/api/core')
  return invoke<PetPackInfo[]>('pet_packs_list')
}

/** 弹出系统选择器选 zip 并导入；取消返回 null（失败抛 PackImportError） */
export async function importPetPackViaDialog(): Promise<PetPackInfo | null> {
  const { open } = await import('@tauri-apps/plugin-dialog')
  const picked = await open({
    multiple: false,
    filters: [{ name: '动作包', extensions: ['zip'] }],
  })
  if (typeof picked !== 'string' || !picked) return null
  const { invoke } = await import('@tauri-apps/api/core')
  return invoke<PetPackInfo>('pet_pack_import', { path: picked })
}

export async function deletePetPack(id: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core')
  await invoke('pet_pack_delete', { id })
}

/** 启用指定动作包；null = 恢复内置形象 */
export async function setEnabledPack(id: string | null): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core')
  await invoke('pet_pack_set_enabled', { id })
}
