import { create } from 'zustand'

/** 官方那种"点放大镜呼出搜索面板"的状态 */
export const useSearchOverlay = create(() => ({ open: false }))

export const openSearchOverlay = () => useSearchOverlay.setState({ open: true })
export const closeSearchOverlay = () => useSearchOverlay.setState({ open: false })
