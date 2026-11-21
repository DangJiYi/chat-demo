// 与服务端本地接口通信的薄封装。所有请求都同源，不需要处理跨域。

/** 前端保存到 localStorage 的占位值：真正的 Key 只在服务端，浏览器永远拿不到 */
export const SERVER_KEY = '__server__'

export type UsageTotals = {
  calls: number
  hitTokens: number
  missTokens: number
  outTokens: number
  cost: number
  cacheSaved: number
  offPeakSaving: number
}

export type UsageResponse = {
  now: { peak: boolean; label: string; nextLabel: string; until: number; since?: number }
  session: UsageTotals
  today: UsageTotals
  month: UsageTotals
  currency: string
}

export type BalanceResponse = {
  configured: boolean
  available?: boolean
  currency?: string
  total?: string
  granted?: string
  toppedUp?: string
  at?: number
  error?: string
}

export type KeyStatus = { configured: boolean; hint: string }

const json = async <T,>(res: Response): Promise<T> => {
  if (!res.ok) {
    throw new Error(`HTTP ${res.status}`)
  }
  return (await res.json()) as T
}

export const getKeyStatus = async (): Promise<KeyStatus> =>
  json<KeyStatus>(await fetch('/local/apikey'))

export const saveApiKey = async (apiKey: string): Promise<KeyStatus> =>
  json<KeyStatus>(
    await fetch('/local/apikey', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ apiKey }),
    }),
  )

export const clearApiKey = async (): Promise<void> => {
  await fetch('/local/apikey', { method: 'DELETE' })
}

export const getUsage = async (session?: string): Promise<UsageResponse> =>
  json<UsageResponse>(await fetch(`/local/usage${session ? `?session=${encodeURIComponent(session)}` : ''}`))

export const getBalance = async (): Promise<BalanceResponse> => {
  const res = await fetch('/local/balance')
  // 余额接口在上游失败时会带错误体，仍然要读出来展示
  try {
    return (await res.json()) as BalanceResponse
  } catch {
    return { configured: true, error: `HTTP ${res.status}` }
  }
}

export type SearchHit = {
  sessionId: string
  title: string
  messageId: string
  role: string
  ts: number
  snippet: string
  offset: number
  length: number
  archived: boolean
}

export type SearchResponse = { query: string; hits: SearchHit[]; scanned: number; tookMs: number }

export const searchMessages = async (query: string, limit = 50): Promise<SearchResponse> =>
  json<SearchResponse>(
    await fetch(`/local/search?q=${encodeURIComponent(query)}&limit=${limit}`),
  )

/** 金额显示：小额保留三位，够看清单次调用的花费 */
export const money = (value: number): string => {
  if (!Number.isFinite(value) || value === 0) return '0.00'
  if (value < 0.01) return value.toFixed(4)
  return value.toFixed(2)
}

// ---- 同步 / 归档 / 体积 ----

export type LocalConfig = {
  peer: string
  intervalSec: number
  archiveDays: number
  imageDays: number
  deviceName: string
  syncPort: number
}

export type SyncStatus = {
  enabled: boolean
  peer: string
  lastOk: number
  lastTry: number
  lastErr: string
  pulled: number
  cursor: number
}

/** /local/status：本机服务状态。localIps 是本机在局域网里的地址，用来告诉对端该填什么。 */
export type DeviceStatus = {
  version: string
  port: number
  syncPort: number
  platform: string
  localIps?: string[]
  deviceName?: string
}

export type StorageInfo = {
  activeSessions: number
  archivedSessions: number
  activeBytes: number
  archiveBytes: number
  mediaBytes: number
  /** 解压到私有目录的前端静态资源（不计入会话数据，但系统会算进用户数据） */
  assetsBytes?: number
  /** app.log 大小 */
  logBytes?: number
}

/** 没有任何会话引用的附件（只统计，不自动删） */
export type OrphanMedia = { count: number; bytes: number }

export const getOrphans = async (): Promise<OrphanMedia> =>
  json<OrphanMedia>(await fetch('/local/orphans'))

export type ArchivedItem = {
  id: string
  month: string
  title: string
  updatedAt: number
  msgCount: number
}

const send = async <T,>(path: string, method: string, body?: unknown): Promise<T> => {
  const res = await fetch('/local' + path, {
    method,
    headers: body === undefined ? undefined : { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  if (!res.ok) {
    throw new Error(`HTTP ${res.status}`)
  }
  const text = await res.text()
  return (text ? JSON.parse(text) : undefined) as T
}

export const getConfig = async (): Promise<LocalConfig> => json<LocalConfig>(await fetch('/local/config'))

export const saveConfig = (patch: Partial<LocalConfig>): Promise<LocalConfig> => send('/config', 'PUT', patch)

export const getSyncStatus = async (): Promise<SyncStatus> => json<SyncStatus>(await fetch('/local/sync/status'))

export const getDeviceStatus = async (): Promise<DeviceStatus> => json<DeviceStatus>(await fetch('/local/status'))

export const syncNow = (): Promise<unknown> => send('/sync/now', 'POST', {})

/** 局域网里可配对的另一台设备 */
export type DiscoveredPeer = { name: string; ip: string; port: number }

export type DiscoverResponse = {
  peers: DiscoveredPeer[]
  self: { name: string; ips: string[]; port: number }
}

/** 广播搜索局域网内的设备（服务端最多等 2 秒） */
export const discoverPeers = async (): Promise<DiscoverResponse> =>
  json<DiscoverResponse>(await fetch('/local/discover'))

/**
 * 发起配对：本机指向对端，同时向对端发一条配对请求。
 * 对端需要在本机界面上点「接受」才生效（避免任意人改你的配置）。
 */
export const pairPeer = (peer: { ip: string; port: number }): Promise<{ peer: string; reverse: string }> =>
  send('/pair', 'POST', { ip: peer.ip, port: peer.port })

/** 待确认的配对请求（像蓝牙那样需要本机接受） */
export type PairRequest = { ip: string; port: number; name: string; at: number }

export const listPairRequests = async (): Promise<PairRequest[]> =>
  json<PairRequest[]>(await fetch('/local/pair/requests'))

/** 接受配对：此时才把对端写进本机配置 */
export const acceptPair = (r: PairRequest): Promise<{ peer: string }> =>
  send('/pair/accept', 'POST', { ip: r.ip, port: r.port })

export const rejectPair = (r: PairRequest): Promise<unknown> =>
  send('/pair/reject', 'POST', { ip: r.ip, port: r.port })

export const getStorage = async (): Promise<StorageInfo> => json<StorageInfo>(await fetch('/local/storage'))

export const archiveNow = (): Promise<{ archived: number; dedupSavedBytes?: number }> =>
  send('/archive/now', 'POST', {})

export const imagesNow = (): Promise<{ scanned: number; shrunk: number; savedBytes: number; skipped: number }> =>
  send('/images/now', 'POST', {})

export const restoreSession = (id: string): Promise<unknown> => send(`/restore/${encodeURIComponent(id)}`, 'POST', {})

export const listArchived = async (): Promise<ArchivedItem[]> => json<ArchivedItem[]>(await fetch('/local/archived'))

/** 字节数转人话 */
export const bytes = (n: number): string => {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`
}

export const deleteSessions = (ids: string[]): Promise<{ deleted: number }> =>
  send('/delete', 'POST', { ids })
