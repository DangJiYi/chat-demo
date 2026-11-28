import { toSessionSummary, type Session, type SessionSummary } from './types.js'

/**
 * 持久化完全交给同源的本地服务（`/local/db/*`），浏览器里不再留副本。
 * 这样会话、附件、归档都落在 `data/` 目录里，可备份、可同步、可用普通工具查看。
 * 12 个导出的签名与原 IndexedDB 版本保持一致，上层组件无需改动。
 */

/** 文件原件按附件的本地 id 存储，所有引用同一 id 的消息共用一份。 */
export type StoredFile = {
  id: string
  blob: Blob
}

const BASE = '/local/db'

const url = (path: string) => BASE + path

/** 统一处理响应：404 交给调用方判断，其余异常直接抛出。 */
const send = async (path: string, init?: RequestInit): Promise<Response> => {
  const response = await fetch(url(path), init)
  if (!response.ok && response.status !== 404) {
    // 把服务端的错误内容带出来：否则只看到 500，不知道是哪个字段/权限出的问题
    let detail = ''
    try {
      detail = (await response.text()).slice(0, 200)
    } catch {
      // 读不到就算了
    }
    throw new Error(
      `${init?.method ?? 'GET'} ${path} 失败: ${response.status}${detail ? ` ${detail}` : ''}`
    )
  }
  return response
}

const putBlob = async (path: string, file: StoredFile): Promise<void> => {
  const { blob } = file
  const headers: Record<string, string> = {
    'Content-Type': blob.type || 'application/octet-stream',
  }
  // File 是 Blob 的子类，导入的附件带着原始文件名，顺手带上便于以后导出。
  if (blob instanceof File) {
    headers['X-File-Name'] = encodeURIComponent(blob.name)
  }
  if (blob.type.startsWith('image/')) {
    headers['X-Image'] = '1'
  }
  await send(path, { method: 'PUT', headers, body: blob })
}

export const loadSessionSummaries = async (): Promise<SessionSummary[]> => {
  const response = await send('/summaries')
  return ((await response.json()) as SessionSummary[]).map(toSessionSummary)
}

export const loadSession = async (id: string): Promise<Session | undefined> => {
  const response = await send(`/session/${encodeURIComponent(id)}`)
  if (response.status === 404) {
    return undefined
  }
  return (await response.json()) as Session
}

export const saveSession = async (session: Session, files: StoredFile[] = []): Promise<void> => {
  // 先落附件字节，再落会话本体：会话写成功时它引用的附件一定已经就位。
  for (const file of files) {
    await putBlob(`/file/${encodeURIComponent(file.id)}`, file)
  }
  await send(`/session/${encodeURIComponent(session.id)}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(session),
  })
}

/** 未打开的会话也可重命名，服务端只改标题字段，不碰消息。 */
export const renameSession = async (id: string, title: string): Promise<void> => {
  await send(`/session/${encodeURIComponent(id)}/rename`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ title }),
  })
}

/** 删除会话；服务端按引用计数清理再无人引用的原件，远端文件不删。 */
export const deleteSession = async (sessionId: string): Promise<void> => {
  await send(`/session/${encodeURIComponent(sessionId)}`, { method: 'DELETE' })
}

export const saveFiles = async (files: StoredFile[]): Promise<void> => {
  for (const file of files) {
    await putBlob(`/file/${encodeURIComponent(file.id)}`, file)
  }
}

export const loadFile = async (id: string): Promise<Blob | undefined> => {
  const response = await send(`/file/${encodeURIComponent(id)}`)
  if (response.status === 404) {
    return undefined
  }
  const blob = await response.blob()
  // 服务端回的是原始 MIME；blob() 会保留 content-type，这里再兜一次空类型。
  if (!blob.type && response.headers.get('content-type')) {
    return blob.slice(0, blob.size, response.headers.get('content-type') as string)
  }
  return blob
}

/** 清空导入暂存：开始导入前清除上次中断遗留的数据，导入失败后清除本次写入。 */
export const clearStaging = async (): Promise<void> => {
  await send('/staging/clear', { method: 'POST' })
}

/**
 * 暂存一个会话。
 *
 * 官方 0.1.4 把它改成了批量的 `addStagingSessions`（IndexedDB 一个事务写完），
 * 我们这边是一个会话一个 HTTP PUT。所以下面那个批量版本只是循环，
 * **不合并成单个请求**：服务端按会话 id 落盘，合并没有收益，反而会加大失败重试粒度。
 */
export const addStagingSession = async (session: Session): Promise<void> => {
  await send(`/staging/session/${encodeURIComponent(session.id)}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(session),
  })
}

/** 批量暂存（官方 0.1.4 的接口名，导入器在用它）。 */
export const addStagingSessions = async (sessions: Session[]): Promise<void> => {
  for (const session of sessions) {
    await addStagingSession(session)
  }
}

export const saveStagingFiles = async (files: StoredFile[]): Promise<void> => {
  for (const file of files) {
    await putBlob(`/staging/file/${encodeURIComponent(file.id)}`, file)
  }
}

/**
 * 图片位于会话 JSON 之后；全部读完后回填具体 MIME，保留归档给出的 isImage 标识。
 *
 * `sessionIds` 在官方实现里用来缩小 IndexedDB 的扫描范围。
 * 我们这份由服务端处理：它本来就按附件 id 匹配所有暂存会话，
 * 结果一致。参数保留只为与官方调用点签名兼容，服务端不需要它。
 */
export const updateStagingFileTypes = async (
  types: ReadonlyMap<string, string>,
  _sessionIds?: ReadonlySet<string>
): Promise<void> => {
  await send('/staging/filetypes', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ types: Object.fromEntries(types) }),
  })
}

/**
 * 导入完成后用暂存替换正式数据：服务端一次替换会话与附件，并清空暂存。
 * 直接返回本次提交的列表，调用方无需在替换成功后再发起可能失败的读取。
 */
export const commitStaging = async (): Promise<SessionSummary[]> => {
  const response = await send('/staging/commit', { method: 'POST' })
  return ((await response.json()) as SessionSummary[]).map(toSessionSummary)
}

/** 本地存储失败不打断对话，只在控制台留痕 */
export const reportStorageError = (action: string) => (error: unknown) => {
  // eslint-disable-next-line no-console
  console.error(`[storage] ${action} failed`, error)
}
