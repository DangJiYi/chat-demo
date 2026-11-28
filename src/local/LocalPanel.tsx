import { useEffect, useMemo, useRef, useState } from 'react'
import { Modal } from '../components/base/Modal.js'
import { Button } from '../components/base/Button.js'
import { showToast } from '../components/base/toastState.js'
import { selectSession } from '../services/chat/store.js'
import {
  acceptPair,
  archiveNow,
  bytes,
  deleteSessions,
  discoverPeers,
  getConfig,
  getDeviceStatus,
  getOrphans,
  getStorage,
  getSyncStatus,
  imagesNow,
  listArchived,
  listPairRequests,
  pairPeer,
  rejectPair,
  restoreSession,
  saveConfig,
  searchMessages,
  type ArchivedItem,
  type DiscoveredPeer,
  type LocalConfig,
  type OrphanMedia,
  type PairRequest,
  type SearchHit,
  type StorageInfo,
  type SyncStatus,
} from './api.js'
import styles from './LocalPanel.module.css'

/** 把 "192.168.1.23:8791" 拆成 IP 与端口，界面上是两格 */
const splitPeer = (peer: string): { host: string; port: string } => {
  const value = peer.trim()
  if (!value) return { host: '', port: '8791' }
  const idx = value.lastIndexOf(':')
  if (idx > 0 && /^\d+$/.test(value.slice(idx + 1))) {
    return { host: value.slice(0, idx), port: value.slice(idx + 1) }
  }
  return { host: value, port: '8791' }
}

const joinPeer = (host: string, port: string): string => {
  const h = host.trim()
  if (!h) return ''
  const p = port.trim() || '8791'
  return `${h.includes(':') ? `[${h}]` : h}:${p}`
}

const dayLabel = (ms: number): string => {
  const d = new Date(ms)
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`
}

/** 一条会话可能命中多条消息，按会话合并，只留第一条命中做片段 */
type Group = { sessionId: string; title: string; archived: boolean; ts: number; hits: number; hit: SearchHit }

const groupHits = (hits: SearchHit[]): Group[] => {
  const map = new Map<string, Group>()
  for (const hit of hits) {
    const found = map.get(hit.sessionId)
    if (found) {
      found.hits += 1
      continue
    }
    map.set(hit.sessionId, {
      sessionId: hit.sessionId,
      title: hit.title,
      archived: hit.archived,
      ts: hit.ts,
      hits: 1,
      hit,
    })
  }
  return [...map.values()]
}

export const LocalPanel = ({ onClose }: { onClose: () => void }) => {
  const [cfg, setCfg] = useState<LocalConfig | null>(null)
  const [storage, setStorage] = useState<StorageInfo | null>(null)
  const [sync, setSync] = useState<SyncStatus | null>(null)
  const [archived, setArchived] = useState<ArchivedItem[]>([])
  const [busy, setBusy] = useState('')

  const [host, setHost] = useState('')
  const [port, setPort] = useState('8791')
  /** 本机在局域网里的地址，填到对端那台机器的「对端 IP」里 */
  const [localIp, setLocalIp] = useState('')
  const [syncPort, setSyncPort] = useState(8791)
  /** 搜索到的近邻设备 */
  const [found, setFound] = useState<DiscoveredPeer[] | null>(null)
  const [scanning, setScanning] = useState(false)
  /** 本机名称 / 端口（可改） */
  const [deviceName, setDeviceName] = useState('')
  const [portDraft, setPortDraft] = useState('8791')
  /** 待本机确认的配对请求 */
  const [requests, setRequests] = useState<PairRequest[]>([])

  /** 未使用附件（只显示，不自动清） */
  const [orphans, setOrphans] = useState<OrphanMedia | null>(null)
  // 运行日志（服务端 data/app.log 的尾部）。轮询 status 时一起拿回来。
  const [logLines, setLogLines] = useState<string[]>([])
  // 服务端版本（/local/status 的 version）。
  // 面板里直接显示它，用来回答一个很具体的问题：“手机上跑的是哪个构建？”
  // 以前只能去看 app.log 或系统设置，两者都不可靠（前者要滚日志，后者是 APK 版本）。
  const [serviceVersion, setServiceVersion] = useState('')

  const [query, setQuery] = useState('')
  const [hits, setHits] = useState<SearchHit[] | null>(null)
  const [searchMs, setSearchMs] = useState(0)
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const wrapRef = useRef<HTMLDivElement>(null)

  const groups = useMemo(() => (hits ? groupHits(hits) : []), [hits])
  const allSelected = groups.length > 0 && groups.every((g) => selected.has(g.sessionId))

  /**
   * 复制本端地址。
   * WebView 里 navigator.clipboard 经常不可用（非安全上下文或缺权限），
   * 所以退回隐藏 textarea + execCommand，再不行就选中文本让用户自己复制。
   */
  const copyLocal = async () => {
    const text = localIp ? `${localIp}:${syncPort}` : ''
    if (!text) {
      showToast('还没获取到本端地址')
      return
    }
    try {
      await navigator.clipboard.writeText(text)
      showToast(`已复制本端地址 ${text}`)
      return
    } catch {
      // 继续尝试兜底方式
    }
    try {
      const area = document.createElement('textarea')
      area.value = text
      area.setAttribute('readonly', '')
      area.style.position = 'fixed'
      area.style.left = '-9999px'
      document.body.appendChild(area)
      area.select()
      area.setSelectionRange(0, text.length)
      const ok = document.execCommand('copy')
      document.body.removeChild(area)
      showToast(ok ? `已复制本端地址 ${text}` : `本端地址：${text}`)
    } catch {
      showToast(`本端地址：${text}`)
    }
  }

  const saveName = () =>
    void run('name', async () => {
      const name = deviceName.trim()
      if (!name) return '名称不能为空'
      await saveConfig({ deviceName: name })
      void refresh()
      return `本机名称已改为「${name}」`
    })

  const saveSyncPort = () =>
    void run('port', async () => {
      const value = Number(portDraft)
      if (!Number.isInteger(value) || value < 1024 || value > 65535) {
        return '端口需在 1024–65535 之间'
      }
      await saveConfig({ syncPort: value })
      void refresh()
      return `端口已改为 ${value}，重启客户端后生效`
    })

  const accept = (r: PairRequest) =>
    void run('accept', async () => {
      await acceptPair(r)
      void refresh()
      return `已与 ${r.name} 配对`
    })

  const reject = (r: PairRequest) =>
    void run('reject', async () => {
      await rejectPair(r)
      void refresh()
      return '已拒绝该配对请求'
    })

  /** 搜索局域网设备并一键配对（免手填两边 IP） */
  const scan = () =>
    void run('scan', async () => {
      setScanning(true)
      try {
        const res = await discoverPeers()
        setFound(res.peers)
        if (res.peers.length === 0) {
          return '没找到其他设备：确认两台在同一局域网、对端也已开启客户端'
        }
        return `找到 ${res.peers.length} 台设备`
      } finally {
        setScanning(false)
      }
    })

  const pair = (peer: DiscoveredPeer) =>
    void run('pair', async () => {
      const res = await pairPeer(peer)
      setHost(peer.ip)
      setPort(String(peer.port))
      void refresh()
      if (res.reverse === 'pending') {
        return `已请求与 ${peer.name} 配对；请在对方设备上点「接受」`
      }
      return `本机已指向 ${peer.name}；未能通知对端（${res.reverse}）`
    })

  const refresh = async () => {
    try {
      const [c, s, sy, ar, dev] = await Promise.all([
        getConfig(),
        getStorage(),
        getSyncStatus(),
        listArchived(),
        getDeviceStatus(),
      ])
      // 孤儿附件统计要扫归档，单独跑，失败不影响主体
      void getOrphans().then(setOrphans).catch(() => setOrphans(null))
      setCfg(c)
      setStorage(s)
      setSync(sy)
      setArchived(ar)
      setLocalIp(dev.localIps?.[0] ?? '')
      setSyncPort(dev.syncPort ?? 8791)
      setServiceVersion(dev.version ?? '')
      setDeviceName(c.deviceName ?? '')
      setPortDraft(String(dev.syncPort ?? 8791))
      // 只取日志数组。不要用整个 status 当指纹：里面的 uptimeSec 每次都变，
      // 会让面板每 4 秒白重绘一次，把正在看的日志滚动位置重置掉。
      setLogLines(dev.log ?? [])
      const split = splitPeer(c.peer)
      setHost(split.host)
      setPort(split.port)
    } catch {
      // 服务没起来时保持空白
    }
  }

  const refreshRequests = async () => {
    try {
      setRequests(await listPairRequests())
    } catch {
      // 服务重启瞬间可能拿不到，忽略
    }
  }

  useEffect(() => {
    wrapRef.current?.scrollTo(0, 0)
    void refresh()
    void refreshRequests()
    // 对端配对请求是异步到达的，面板开着时定时看一眼
    const timer = setInterval(() => void refreshRequests(), 4000)
    return () => clearInterval(timer)
  }, [])

  const patch = async (next: Partial<LocalConfig>) => {
    if (!cfg) return
    setCfg({ ...cfg, ...next })
    try {
      setCfg(await saveConfig(next))
    } catch {
      showToast('保存失败')
    }
  }

  const savePeer = () => void patch({ peer: joinPeer(host, port) })

  const run = async (name: string, task: () => Promise<string>) => {
    setBusy(name)
    try {
      showToast(await task())
      await refresh()
    } catch {
      showToast('操作失败，看看服务是否还在')
    } finally {
      setBusy('')
    }
  }

  const search = async () => {
    const q = query.trim()
    if (!q) {
      setHits(null)
      setSelected(new Set())
      return
    }
    try {
      const res = await searchMessages(q)
      setHits(res.hits)
      setSearchMs(res.tookMs)
      setSelected(new Set())
    } catch {
      showToast('搜索失败')
    }
  }

  const toggle = (id: string) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const toggleAll = () => {
    setSelected(allSelected ? new Set() : new Set(groups.map((g) => g.sessionId)))
  }

  const open = async (group: Group) => {
    if (group.archived) {
      await restoreSession(group.sessionId)
      showToast('已从归档恢复，正在打开')
      setTimeout(() => location.reload(), 400)
      return
    }
    selectSession(group.sessionId)
    onClose()
  }

  const restoreSelected = () =>
    void run('restore', async () => {
      const ids = groups.filter((g) => g.archived && selected.has(g.sessionId)).map((g) => g.sessionId)
      if (ids.length === 0) return '选中的都不在归档里'
      for (const id of ids) await restoreSession(id)
      setTimeout(() => location.reload(), 600)
      return `已恢复 ${ids.length} 个会话`
    })

  const deleteSelected = () =>
    void run('delete', async () => {
      const ids = [...selected]
      if (ids.length === 0) return '还没选'
      const res = await deleteSessions(ids)
      setHits((prev) => (prev ? prev.filter((h) => !selected.has(h.sessionId)) : prev))
      setSelected(new Set())
      return `已删除 ${res.deleted} 项`
    })

  return (
    <Modal title="本地与同步" onClose={onClose}>
      <div className={styles.wrap} ref={wrapRef}>
        {/* ---------- 同步 ---------- */}
        <section className={styles.section}>
          <h3 className={styles.h}>本机</h3>
          <div className={styles.fieldRow}>
            <label className={styles.field}>
              <span className={styles.label}>名称</span>
              <input
                className={styles.input}
                placeholder="这台设备的名称"
                value={deviceName}
                onChange={(e) => setDeviceName(e.target.value)}
                onBlur={saveName}
              />
            </label>
            <label className={`${styles.field} ${styles.narrow}`}>
              <span className={styles.label}>端口</span>
              <input
                className={styles.input}
                inputMode="numeric"
                value={portDraft}
                onChange={(e) => setPortDraft(e.target.value)}
                onBlur={saveSyncPort}
              />
            </label>
          </div>
          <div className={styles.fieldRow}>
            <label className={styles.field}>
              <span className={styles.label}>本端地址</span>
              <input
                className={styles.input}
                readOnly
                value={localIp ? `${localIp}:${syncPort}` : '未获取到（没连局域网？）'}
              />
            </label>
            <Button variant="outlined" onClick={() => void copyLocal()} disabled={!localIp}>
              复制
            </Button>
          </div>
        </section>

        <section className={styles.section}>
          <h3 className={styles.h}>同步</h3>
          <div className={styles.fieldRow}>
            <Button variant="outlined" onClick={scan} disabled={scanning}>
              {scanning ? '搜索中…' : '搜索附近设备'}
            </Button>
          </div>
          {found !== null ? (
            found.length === 0 ? (
              <p className={styles.hint}>没找到其他设备，确认两台在同一局域网。</p>
            ) : (
              <div className={styles.peerList}>
                {found.map((peer) => (
                  <button
                    key={`${peer.ip}:${peer.port}`}
                    type="button"
                    className={styles.peerItem}
                    onClick={() => pair(peer)}
                  >
                    <span className={styles.peerName}>{peer.name}</span>
                    <span className={styles.peerAddr}>
                      {peer.ip}:{peer.port}
                    </span>
                    <span className={styles.peerAction}>配对</span>
                  </button>
                ))}
              </div>
            )
          ) : null}
          {requests.length > 0 ? (
            <div className={styles.peerList}>
              {requests.map((r) => (
                <div key={`${r.ip}:${r.port}`} className={styles.peerItem}>
                  <span className={styles.peerName}>{r.name}</span>
                  <span className={styles.peerAddr}>
                    {r.ip}:{r.port}
                  </span>
                  <button type="button" className={styles.peerAction} onClick={() => accept(r)}>
                    接受
                  </button>
                  <button type="button" className={styles.peerReject} onClick={() => reject(r)}>
                    拒绝
                  </button>
                </div>
              ))}
            </div>
          ) : null}
          <div className={styles.fieldRow}>
            <label className={styles.field}>
              <span className={styles.label}>对端 IP</span>
              <input
                className={styles.input}
                placeholder="192.168.1.23"
                value={host}
                onChange={(e) => setHost(e.target.value)}
                onBlur={savePeer}
              />
            </label>
            <label className={`${styles.field} ${styles.narrow}`}>
              <span className={styles.label}>端口</span>
              <input
                className={styles.input}
                inputMode="numeric"
                placeholder="8791"
                value={port}
                onChange={(e) => setPort(e.target.value)}
                onBlur={savePeer}
              />
            </label>
          </div>
          <div className={styles.fieldRow}>
            <label className={styles.field}>
              <span className={styles.label}>间隔</span>
              <select
                className={styles.input}
                value={cfg?.intervalSec ?? 3}
                onChange={(e) => void patch({ intervalSec: Number(e.target.value) })}
              >
                <option value={3}>3 秒</option>
                <option value={5}>5 秒</option>
                <option value={10}>10 秒</option>
              </select>
            </label>
            <Button variant="outlined" onClick={() => void run('sync', async () => '已同步')}>
              {busy === 'sync' ? '同步中…' : '立即同步'}
            </Button>
          </div>
          <p className={styles.hint}>
            {sync?.peer
              ? `对端 ${sync.peer} · ${sync.lastErr ? `上次出错：${sync.lastErr}` : `上次成功 ${new Date(sync.lastOk).toLocaleTimeString()}`} · 已拉取 ${sync.pulled} 条`
              : '两台机器互相填对方的 IP 与端口（默认 8791）即可'}
          </p>
        </section>

        {/* ---------- 归档 ---------- */}
        <section className={styles.section}>
          <h3 className={styles.h}>归档</h3>
          <div className={styles.inlineRow}>
            <span className={styles.label}>超过</span>
            <input
              className={`${styles.input} ${styles.number}`}
              type="number"
              min={7}
              max={14}
              value={cfg?.archiveDays ?? 7}
              onChange={(e) => void patch({ archiveDays: Number(e.target.value) || 7 })}
            />
            <span className={styles.label}>天自动归档</span>
          </div>
          <div className={styles.inlineRow}>
            <span className={styles.label}>超过</span>
            <input
              className={`${styles.input} ${styles.number}`}
              type="number"
              min={0}
              max={60}
              value={cfg?.imageDays ?? 14}
              onChange={(e) => void patch({ imageDays: Number(e.target.value) || 0 })}
            />
            <span className={styles.label}>天压缩图片</span>
          </div>
          <div className={styles.fieldRow}>
            <Button
              variant="outlined"
              onClick={() =>
                void run('archive', async () => {
                  const res = await archiveNow()
                  void refresh()
                  const freed = res.dedupSavedBytes ?? 0
                  // 归档去重是“立即整理”顺带做的，有释放就报出来
                  return freed > 0
                    ? `归档了 ${res.archived} 个会话，去重释放 ${bytes(freed)}`
                    : `归档了 ${res.archived} 个会话`
                })
              }
            >
              {busy === 'archive' ? '整理中…' : '立即整理'}
            </Button>
            <Button
              variant="outlined"
              onClick={() =>
                void run('images', async () => {
                  const r = await imagesNow()
                  return `压缩 ${r.shrunk} 张，省下 ${bytes(r.savedBytes)}`
                })
              }
            >
              {busy === 'images' ? '压缩中…' : '立即压缩图片'}
            </Button>
          </div>
          <div className={styles.statsRow}>
            <div className={styles.stat}>
              <span className={styles.statValue}>{storage?.activeSessions ?? 0}</span>
              <span className={styles.statLabel}>活跃会话 · {bytes(storage?.activeBytes ?? 0)}</span>
            </div>
            <div className={styles.stat}>
              <span className={styles.statValue}>{storage?.archivedSessions ?? 0}</span>
              <span className={styles.statLabel}>已归档 · {bytes(storage?.archiveBytes ?? 0)}</span>
            </div>
            <div className={styles.stat}>
              <span className={styles.statValue}>{bytes(storage?.mediaBytes ?? 0)}</span>
              <span className={styles.statLabel}>图片占用</span>
            </div>
          </div>
          {/* 系统设置里的“用户数据”还包含解压出来的前端资源和日志，这里一并报出来，免得对不上账 */}
          <p className={styles.hint}>
            界面文件 {bytes(storage?.assetsBytes ?? 0)}
            {storage?.assetsFiles != null ? `（${storage.assetsFiles} 个文件）` : ''}
            {' · 日志 '}
            {bytes(storage?.logBytes ?? 0)}
            {orphans && orphans.count > 0
              ? ` · 未使用附件 ${orphans.count} 个 ${bytes(orphans.bytes)}`
              : ''}
          </p>
          {/* 诊断：目录名带版本戳，正常约 111 个文件；偏大或名字异常就是旧副本残留 */}
          {storage?.assetsDir ? (
            <p className={styles.hint}>
              资源目录 {storage.assetsDir}
              {storage.assetsBreakdown?.length
                ? ` · ${storage.assetsBreakdown.join(' · ')}`
                : ''}
            </p>
          ) : null}
        </section>

        {/* ---------- 历史搜索 ---------- */}
        <section className={styles.section}>
          <h3 className={styles.h}>历史搜索</h3>
          <div className={styles.searchBar}>
            <svg className={styles.searchIcon} viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
              <circle cx="11" cy="11" r="6.5" fill="none" stroke="currentColor" strokeWidth="2" />
              <path d="M16.2 16.2L21 21" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
            </svg>
            <input
              className={styles.searchInput}
              placeholder="搜索聊天记录，中英文都行"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') void search()
              }}
            />
            <button type="button" className={styles.searchButton} onClick={() => void search()}>
              搜索
            </button>
          </div>

          {hits && (
            <>
              <div className={styles.toolbar}>
                <label className={styles.checkAll}>
                  <input type="checkbox" checked={allSelected} onChange={toggleAll} />
                  全选
                </label>
                <span className={styles.toolbarInfo}>
                  {groups.length} 会话 · {hits.length} 命中 · {searchMs}ms
                  {selected.size > 0 ? ` · 已选 ${selected.size}` : ''}
                </span>
                <span className={styles.toolbarActions}>
                  <button type="button" className={styles.textButton} disabled={!selected.size} onClick={restoreSelected}>
                    恢复
                  </button>
                  <button
                    type="button"
                    className={`${styles.textButton} ${styles.danger}`}
                    disabled={!selected.size}
                    onClick={deleteSelected}
                  >
                    删除
                  </button>
                </span>
              </div>

              <ul className={styles.hits}>
                {groups.map((group) => (
                  <li key={group.sessionId} className={styles.hitRow}>
                    <input
                      type="checkbox"
                      className={styles.check}
                      checked={selected.has(group.sessionId)}
                      onChange={() => toggle(group.sessionId)}
                      aria-label={`选择 ${group.title}`}
                    />
                    <button type="button" className={styles.hit} onClick={() => void open(group)}>
                      <span className={styles.hitTitle}>
                        <span className={styles.hitName}>{group.title || '未命名会话'}</span>
                        {group.archived && <em className={styles.badge}>已归档</em>}
                        {group.hits > 1 && <em className={styles.badge}>{group.hits} 条命中</em>}
                        <span className={styles.hitDate}>{dayLabel(group.ts)}</span>
                      </span>
                      <span className={styles.snippet}>
                        {group.hit.snippet.slice(0, group.hit.offset)}
                        <mark>{group.hit.snippet.slice(group.hit.offset, group.hit.offset + group.hit.length)}</mark>
                        {group.hit.snippet.slice(group.hit.offset + group.hit.length)}
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
              {groups.length === 0 && <p className={styles.hint}>没有命中，换个词试试</p>}
            </>
          )}
        </section>

        {/* ---------- 运行日志 ---------- */}
        {/*
          把静默失败变成可见失败的入口。点按钮没反应、界面报错但控制台干净时，
          先看这里：接口 4xx、前端 JS 异常、未处理的 Promise 拒绝都会落进来。
        */}
        <section className={styles.section}>
          <h3 className={styles.h}>
            运行日志
            {serviceVersion ? (
              <span style={{ fontWeight: 400, opacity: 0.6, marginLeft: 8 }}>
                v{serviceVersion}
              </span>
            ) : null}
          </h3>
          <p className={styles.hint}>
            接口拒绝、前端异常、录音失败都在这里（最近 40 行，每 4 秒自动刷新）。
            上方的 vX.Y.Z 是后端版本，用来确认装的是哪个构建。
          </p>
          {logLines.length > 0 ? (
            <pre
              style={{
                maxHeight: 220,
                overflow: 'auto',
                margin: '8px 0 0',
                padding: 10,
                borderRadius: 8,
                background: 'rgba(127,127,127,0.10)',
                fontSize: 12,
                lineHeight: 1.6,
                whiteSpace: 'pre-wrap',
                wordBreak: 'break-all',
                userSelect: 'text',
              }}
            >
              {logLines.join('\n')}
            </pre>
          ) : (
            <p className={styles.hint}>暂无日志。</p>
          )}
          <div className={styles.fieldRow} style={{ marginTop: 8 }}>
            <Button variant="outlined" onClick={() => void refresh()}>
              刷新
            </Button>
            <Button
              variant="outlined"
              onClick={() => {
                // 复制到剪贴板，便于直接贴给别人看
                const text = logLines.join('\n')
                if (!text) return
                void navigator.clipboard?.writeText(text).catch(() => {})
              }}
            >
              复制
            </Button>
          </div>
        </section>

        {/* ---------- 归档清单 ---------- */}
        {archived.length > 0 && (
          <section className={styles.section}>
            <h3 className={styles.h}>归档会话（{archived.length}）</h3>
            <ul className={styles.hits}>
              {archived.slice(0, 30).map((item) => (
                <li key={item.id}>
                  <button
                    type="button"
                    className={styles.hit}
                    onClick={() =>
                      void run('restore', async () => {
                        await restoreSession(item.id)
                        setTimeout(() => location.reload(), 400)
                        return '已恢复'
                      })
                    }
                  >
                    <span className={styles.hitTitle}>
                      <span className={styles.hitName}>{item.title || '未命名会话'}</span>
                      <span className={styles.hitDate}>
                        {dayLabel(item.updatedAt)} · {item.msgCount} 条
                      </span>
                    </span>
                    <span className={styles.hint}>点击恢复到活跃库</span>
                  </button>
                </li>
              ))}
            </ul>
          </section>
        )}
      </div>
    </Modal>
  )
}
