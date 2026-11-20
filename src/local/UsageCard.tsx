import { useEffect, useState } from 'react'
import { getBalance, getUsage, money, type BalanceResponse, type UsageResponse } from './api.js'
import styles from './UsageCard.module.css'

const hhmm = (ms: number): string => {
  const d = new Date(ms)
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

/** 与原插件 countdownText 一致：超过一天说"天/小时/分"，否则说"分/秒" */
const durationText = (ms: number): string => {
  const minutes = Math.max(1, Math.ceil(ms / 60000))
  const days = Math.floor(minutes / 1440)
  const hours = Math.floor((minutes % 1440) / 60)
  const mins = minutes % 60
  if (days > 0) {
    return `${days}天${hours ? `${hours}小时` : ''}${mins ? `${mins}分` : ''}`
  }
  if (hours > 0) {
    return `${hours}小时${mins ? `${mins}分` : ''}`
  }
  return `${mins}分`
}

const balanceText = (balance: BalanceResponse | null): string => {
  if (!balance) return '…'
  if (!balance.configured) return '未配置 Key'
  if (balance.error) return '查询失败'
  return `${balance.currency === 'USD' ? '$' : '¥'}${balance.total ?? '—'}`
}

/**
 * 侧边栏底部的用量与余额卡片。
 * 峰谷那一段按 dsh-cost-meter 的 peak 组件来：彩色圆点 + 时段名、进度条、开始/切换时间。
 */
export const UsageCard = () => {
  const [usage, setUsage] = useState<UsageResponse | null>(null)
  const [balance, setBalance] = useState<BalanceResponse | null>(null)
  const [now, setNow] = useState(() => Date.now())

  useEffect(() => {
    let alive = true
    const loadUsage = async () => {
      try {
        const next = await getUsage()
        if (alive) setUsage(next)
      } catch {
        // 服务没起来时静默
      }
    }
    const loadBalance = async () => {
      try {
        const next = await getBalance()
        if (alive) setBalance(next)
      } catch {
        if (alive) setBalance({ configured: true, error: 'network' })
      }
    }
    void loadUsage()
    void loadBalance()
    const usageTimer = setInterval(() => void loadUsage(), 30_000)
    const balanceTimer = setInterval(() => void loadBalance(), 60_000)
    const tick = setInterval(() => setNow(Date.now()), 1000)
    return () => {
      alive = false
      clearInterval(usageTimer)
      clearInterval(balanceTimer)
      clearInterval(tick)
    }
  }, [])

  const peak = usage?.now.peak ?? false
  const since = usage?.now.since ?? now
  const until = usage?.now.until ?? now
  const span = Math.max(1, until - since)
  const progress = Math.min(100, Math.max(0, ((now - since) / span) * 100))
  const remain = Math.max(0, until - now)
  const nextLabel = usage?.now.nextLabel ?? '闲时'

  return (
    <div className={styles.card}>
      <div className={`${styles.peak} ${peak ? styles.on : styles.off}`}>
        <div className={styles.head}>
          <span className={styles.phase}>
            <i className={styles.dot} aria-hidden="true" />
            {usage ? usage.now.label : '…'}
          </span>
          <span className={styles.time}>剩余 {durationText(remain)}</span>
        </div>
        <div
          className={styles.track}
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(progress)}
          aria-label="当前计价时段已过进度"
          title={`${hhmm(since)} 开始，本时段已过 ${progress.toFixed(1)}%`}
        >
          <span className={styles.fill} style={{ width: `${progress}%` }} />
        </div>
        <div className={styles.meta}>
          <span>{hhmm(since)} 开始</span>
          <span>
            {hhmm(until)} 转{nextLabel}
          </span>
        </div>
      </div>

      <div className={styles.row}>
        <span className={styles.label}>今日</span>
        <span className={styles.value}>¥{usage ? money(usage.today.cost) : '…'}</span>
        <span className={styles.label}>本月</span>
        <span className={styles.value}>¥{usage ? money(usage.month.cost) : '…'}</span>
      </div>
      <div className={styles.row}>
        <span className={styles.label}>余额</span>
        <span className={styles.value}>{balanceText(balance)}</span>
      </div>
    </div>
  )
}
