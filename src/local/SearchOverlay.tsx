import { useEffect, useMemo, useRef, useState } from 'react'
import { selectSession } from '../services/chat/store.js'
import { restoreSession, searchMessages, type SearchHit } from './api.js'
import { closeSearchOverlay, useSearchOverlay } from './searchOverlayState.js'
import styles from './SearchOverlay.module.css'

/** 官方列表那种相对日期：今天 / 昨天 / N 天前 / 年月日 */
const relativeDay = (ms: number): string => {
  const day = 24 * 3600 * 1000
  const now = new Date()
  const start = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime()
  const target = new Date(ms)
  const targetStart = new Date(target.getFullYear(), target.getMonth(), target.getDate()).getTime()
  const diff = Math.round((start - targetStart) / day)
  if (diff <= 0) return '今天'
  if (diff === 1) return '昨天'
  if (diff < 7) return `${diff} 天前`
  return `${target.getFullYear()}-${String(target.getMonth() + 1).padStart(2, '0')}-${String(target.getDate()).padStart(2, '0')}`
}

type Group = { key: string; title: string; archived: boolean; ts: number; hits: number; hit: SearchHit }

const groupBySession = (hits: SearchHit[]): Group[] => {
  const map = new Map<string, Group>()
  for (const hit of hits) {
    const found = map.get(hit.sessionId)
    if (found) {
      found.hits += 1
      continue
    }
    map.set(hit.sessionId, {
      key: hit.sessionId,
      title: hit.title,
      archived: hit.archived,
      ts: hit.ts,
      hits: 1,
      hit,
    })
  }
  return [...map.values()]
}

const ChatIcon = () => (
  <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
    <path
      d="M4.2 6.4A2.4 2.4 0 0 1 6.6 4h10.8a2.4 2.4 0 0 1 2.4 2.4v7.2a2.4 2.4 0 0 1-2.4 2.4H9.6L5.4 19.4V16.1A2.4 2.4 0 0 1 4.2 14z"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinejoin="round"
    />
  </svg>
)

/** 官方那种顶部搜索面板：图标 + 输入 + 清空，下面是按会话归组的结果 */
export const SearchOverlay = () => {
  const open = useSearchOverlay((state) => state.open)
  const [query, setQuery] = useState('')
  const [hits, setHits] = useState<SearchHit[]>([])
  const [busy, setBusy] = useState(false)
  const [tookMs, setTookMs] = useState(0)
  const [selectedIndex, setSelectedIndex] = useState(0)
  const inputRef = useRef<HTMLInputElement>(null)

  const groups = useMemo(() => groupBySession(hits), [hits])

  useEffect(() => {
    if (!open) {
      setQuery('')
      setHits([])
      return
    }
    // 打开后聚焦输入框，和官方一致
    const timer = setTimeout(() => inputRef.current?.focus(), 30)
    return () => clearTimeout(timer)
  }, [open])

  useEffect(() => {
    if (!open) return
    const q = query.trim()
    if (!q) {
      setHits([])
      setTookMs(0)
      return
    }
    let alive = true
    setBusy(true)
    const timer = setTimeout(() => {
      searchMessages(q, 100)
        .then((res) => {
          if (!alive) return
          setHits(res.hits)
          setTookMs(res.tookMs)
          setSelectedIndex(0)
        })
        .catch(() => {
          if (alive) setHits([])
        })
        .finally(() => {
          if (alive) setBusy(false)
        })
    }, 180)
    return () => {
      alive = false
      clearTimeout(timer)
      setBusy(false)
    }
  }, [query, open])

  useEffect(() => {
    if (!open) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        closeSearchOverlay()
        return
      }
      if (event.key === 'ArrowDown') {
        event.preventDefault()
        setSelectedIndex((i) => Math.min(groups.length - 1, i + 1))
      } else if (event.key === 'ArrowUp') {
        event.preventDefault()
        setSelectedIndex((i) => Math.max(0, i - 1))
      } else if (event.key === 'Enter' && groups[selectedIndex]) {
        void choose(groups[selectedIndex])
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [open, groups, selectedIndex])

  const choose = async (group: Group) => {
    if (group.archived) {
      await restoreSession(group.key)
      closeSearchOverlay()
      setTimeout(() => location.reload(), 300)
      return
    }
    selectSession(group.key)
    closeSearchOverlay()
  }

  if (!open) return null

  return (
    <div
      className={styles.backdrop}
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) closeSearchOverlay()
      }}
    >
      <div className={styles.panel} role="dialog" aria-label="搜索聊天记录">
        <div className={styles.inputRow}>
          <svg className={styles.searchIcon} viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
            <circle cx="11" cy="11" r="6.5" fill="none" stroke="currentColor" strokeWidth="1.8" />
            <path d="M16.2 16.2L21 21" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
          </svg>
          <input
            ref={inputRef}
            className={styles.input}
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="搜索聊天记录"
            spellCheck={false}
          />
          {query ? (
            <button
              type="button"
              className={styles.clear}
              aria-label="清空"
              onClick={() => {
                setQuery('')
                inputRef.current?.focus()
              }}
            >
              <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
                <path d="M6 6l12 12M18 6L6 18" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
              </svg>
            </button>
          ) : null}
        </div>

        <div className={styles.list}>
          {groups.map((group, index) => (
            <button
              key={group.key}
              type="button"
              className={index === selectedIndex ? `${styles.row} ${styles.rowActive}` : styles.row}
              onMouseEnter={() => setSelectedIndex(index)}
              onClick={() => void choose(group)}
            >
              <span className={styles.avatar}>
                <ChatIcon />
              </span>
              <span className={styles.body}>
                <span className={styles.titleRow}>
                  <span className={styles.title}>{group.title || '未命名会话'}</span>
                  {group.archived && <em className={styles.badge}>已归档</em>}
                  {group.hits > 1 && <em className={styles.badge}>{group.hits} 条命中</em>}
                  <span className={styles.day}>{relativeDay(group.ts)}</span>
                </span>
                <span className={styles.snippet}>
                  {group.hit.snippet.slice(0, group.hit.offset)}
                  <mark className={styles.mark}>
                    {group.hit.snippet.slice(group.hit.offset, group.hit.offset + group.hit.length)}
                  </mark>
                  {group.hit.snippet.slice(group.hit.offset + group.hit.length)}
                </span>
              </span>
            </button>
          ))}

          {!busy && query.trim() && groups.length === 0 && (
            <div className={styles.empty}>没有匹配的聊天记录</div>
          )}
          {!query.trim() && <div className={styles.empty}>输入关键词，中英文都可以</div>}
        </div>

        {query.trim() && groups.length > 0 && (
          <div className={styles.footer}>
            {groups.length} 个对话 · {hits.length} 条命中 · {tookMs}ms · ↑↓ 选择，Enter 打开，Esc 关闭
          </div>
        )}
      </div>
    </div>
  )
}
