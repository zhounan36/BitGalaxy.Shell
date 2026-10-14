/**
 * 动作包宿主状态：加载当前启用的用户包并构建 PackRuntime。
 * 浏览器宿主恒为内置 runtime（用户包能力降级）。
 */
import { useCallback, useEffect, useState } from 'react'
import { createAssetUrlFactory, isPackCapable, listPetPacks } from './packApi'
import { createPackRuntime, type PackRuntime, type PetPackInfo } from './packRuntime'

export interface PetPackState {
  /** 当前渲染使用的 runtime（内置 or 用户包覆盖） */
  runtime: PackRuntime
  /** 当前启用的用户包；null = 内置 */
  activePack: PetPackInfo | null
  /** 全部用户包（管理面板列表用） */
  packs: PetPackInfo[]
  /** 变更（导入/删除/启用）后刷新 */
  refresh: () => Promise<void>
}

export function usePetPack(): PetPackState {
  const [state, setState] = useState<Omit<PetPackState, 'refresh'>>(() => ({
    runtime: createPackRuntime(null),
    activePack: null,
    packs: [],
  }))

  const refresh = useCallback(async () => {
    if (!isPackCapable) return
    try {
      const [toUrl, packs] = await Promise.all([createAssetUrlFactory(), listPetPacks()])
      const activePack = packs.find((p) => p.active) ?? null
      setState({
        runtime: createPackRuntime(activePack, toUrl),
        activePack,
        packs,
      })
    } catch (err) {
      // 动作包读取失败不阻断内置渲染
      console.error('load pet packs failed', err)
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  return { ...state, refresh }
}
