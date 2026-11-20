import { create } from 'zustand'

/** 本地与同步弹窗的开合状态，侧边栏按钮和上限都读它 */
export const useLocalPanel = create(() => ({ open: false }))

export const openLocalPanel = () => useLocalPanel.setState({ open: true })
export const closeLocalPanel = () => useLocalPanel.setState({ open: false })
