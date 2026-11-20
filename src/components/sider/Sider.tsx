import clsx from 'clsx'
import { useEffect, useMemo, useState } from 'react'
import { useTranslate } from '../../i18n/index.js'
import {
  IconCheckOutline16,
  IconCloseOutline16,
  IconDataOutline16,
  IconPanelLeftOutline16,
  IconSearchOutline16,
  IconSettingsOutline16,
  NewChatIcon,
} from '../base/icons.js'
import { setSiderCollapsed } from '../../services/settings.js'
import { groupSessions } from '../../services/chat/sessionGroups.js'
import { newChat, removeSession, useChatStore } from '../../services/chat/store.js'
import type { SessionSummary } from '../../services/chat/types.js'
import { Button } from '../base/Button.js'
import { IconButton } from '../base/IconButton.js'
import { Modal } from '../base/Modal.js'
import { showToast } from '../base/toastState.js'
import { openSettings } from '../settings/settingsDialogState.js'
import { closeMobileSider, useMobileSider } from './mobileSiderState.js'
import { SessionItem } from './SessionItem.js'
import { UsageCard } from '../../local/UsageCard.js'
import { openLocalPanel } from '../../local/localPanelState.js'
import { openSearchOverlay } from '../../local/searchOverlayState.js'
import styles from './Sider.module.css'

/** 展开的侧边栏：名称与收起按钮、开启新对话、按时间分组的会话列表、底部设置；移动端放在抽屉里，收起按钮关闭抽屉 */
export const Sider = ({ mobile = false }: { mobile?: boolean }) => {
  const t = useTranslate()
  const sessions = useChatStore((state) => state.sessions)
  const activeId = useChatStore((state) => state.activeId)
  const [now, setNow] = useState(Date.now)
  const groups = useMemo(() => groupSessions(sessions, now, t), [sessions, now, t])
  const [deleting, setDeleting] = useState<SessionSummary | null>(null)
  const [deletePending, setDeletePending] = useState(false)
  // 多选模式：官方那套"选中若干对话再批量处理"
  const [selectMode, setSelectMode] = useState(false)
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const [confirmBulk, setConfirmBulk] = useState(false)

  const exitSelect = () => {
    setSelectMode(false)
    setSelected(new Set())
  }
  const toggleSelect = (id: string) =>
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  const allIds = groups.flatMap((group) => group.sessions.map((session) => session.id))
  const allSelected = allIds.length > 0 && allIds.every((id) => selected.has(id))
  const bulkDelete = async () => {
    for (const id of [...selected]) {
      await removeSession(id)
    }
    setConfirmBulk(false)
    exitSelect()
  }

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout>
    const refresh = () => {
      clearTimeout(timer)
      const time = Date.now()
      setNow(time)
      const midnight = new Date(time).setHours(24, 0, 0, 0)
      timer = setTimeout(refresh, midnight - time)
    }
    const onVisibilityChange = () => {
      if (document.visibilityState === 'visible') {
        refresh()
      }
    }
    refresh()
    document.addEventListener('visibilitychange', onVisibilityChange)
    window.addEventListener('focus', refresh)
    return () => {
      clearTimeout(timer)
      document.removeEventListener('visibilitychange', onVisibilityChange)
      window.removeEventListener('focus', refresh)
    }
  }, [])

  return (
    <div className={clsx(styles.sider, mobile && styles.mobile)}>
      {selectMode ? (
        <div className={styles.top}>
          <div className={styles.appName}>已选择 {selected.size} 个对话</div>
          <IconButton size="m" icon={<IconCloseOutline16 />} tooltip="退出多选" tooltipPlacement="bottom" onClick={exitSelect} />
        </div>
      ) : (
        <div className={styles.top}>
          <div className={styles.appName}>{t('appName')}</div>
          <IconButton
            size="m"
            icon={<IconSearchOutline16 />}
            tooltip="搜索聊天记录"
            tooltipPlacement="bottom"
            onClick={() => openSearchOverlay()}
          />
          <IconButton
            size="m"
            icon={<IconPanelLeftOutline16 />}
            tooltip={t('collapseSiderbarButton')}
            tooltipPlacement="bottom"
            onClick={() => (mobile ? closeMobileSider() : setSiderCollapsed(true))}
          />
        </div>
      )}
      <button
        type="button"
        className={styles.newChat}
        onClick={() => {
          newChat()
          closeMobileSider()
        }}
      >
        <span className={styles.newChatIcon}>
          <NewChatIcon />
        </span>
        {t('newChat')}
      </button>
      <div className={styles.listWrap}>
        <div className={styles.list}>
          {groups.map((group, index) => (
            <div key={group.key} className={styles.section}>
              <div className={styles.sectionHeaderRow}>
                <span className={styles.sectionHeader}>{group.label}</span>
                {index === 0 && !selectMode ? (
                  <button
                    type="button"
                    className={styles.selectEntry}
                    onClick={() => setSelectMode(true)}
                    title="多选"
                  >
                    <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
                      <path d="M4 7.2l1.7 1.7L9 5.6" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
                      <path d="M12 7h8M4 13.5h16M4 18.5h16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" opacity="0.55" />
                    </svg>
                  </button>
                ) : null}
              </div>
              {group.sessions.map((session) =>
                selectMode ? (
                  <button
                    key={session.id}
                    type="button"
                    className={styles.selectRow}
                    onClick={() => toggleSelect(session.id)}
                  >
                    <span className={selected.has(session.id) ? styles.checkOn : styles.checkOff}>
                      {selected.has(session.id) ? <IconCheckOutline16 /> : null}
                    </span>
                    <span className={styles.selectTitle}>{session.title || t('untitledChat')}</span>
                  </button>
                ) : (
                  <SessionItem
                    key={session.id}
                    session={session}
                    active={session.id === activeId}
                    onDelete={() => setDeleting(session)}
                  />
                )
              )}
            </div>
          ))}
        </div>
      </div>
      {selectMode && (
        <div className={styles.selectBar}>
          <button
            type="button"
            className={styles.selectAction}
            onClick={() => setSelected(allSelected ? new Set() : new Set(allIds))}
          >
            {allSelected ? '取消全选' : '全选'}
          </button>
          <button
            type="button"
            className={clsx(styles.selectAction, styles.selectDanger)}
            disabled={selected.size === 0}
            onClick={() => setConfirmBulk(true)}
          >
            删除{selected.size ? ` (${selected.size})` : ''}
          </button>
        </div>
      )}
      <UsageCard />
      <div className={styles.bottom}>
        <button type="button" className={styles.settings} onClick={() => openLocalPanel()}>
          <span className={styles.settingsIcon}>
            <IconDataOutline16 />
          </span>
          <span className={styles.settingsLabel}>本地与同步</span>
        </button>
        <button type="button" className={styles.settings} onClick={() => openSettings('general')}>
          <span className={styles.settingsIcon}>
            <IconSettingsOutline16 />
          </span>
          <span className={styles.settingsLabel}>{t('settings')}</span>
        </button>
      </div>
      {confirmBulk ? (
        <Modal
          title="删除对话"
          onClose={() => setConfirmBulk(false)}
          footer={
            <>
              <Button variant="outlined" onClick={() => setConfirmBulk(false)}>
                {t('cancel')}
              </Button>
              <Button variant="danger" onClick={() => void bulkDelete()}>
                {t('delete')}
              </Button>
            </>
          }
        >
          将删除选中的 {selected.size} 个对话，不可恢复。
        </Modal>
      ) : null}
      {deleting ? (
        <Modal
          title={t('deleteSessionModalContentTitle')}
          onClose={() => {
            if (!deletePending) {
              setDeleting(null)
            }
          }}
          footer={
            <>
              <Button variant="outlined" disabled={deletePending} onClick={() => setDeleting(null)}>
                {t('cancel')}
              </Button>
              <Button
                variant="danger"
                disabled={deletePending}
                onClick={async () => {
                  setDeletePending(true)
                  try {
                    if (await removeSession(deleting.id)) {
                      setDeleting(null)
                    } else {
                      showToast({ key: 'sessionDeleteFailed', type: 'error' })
                    }
                  } finally {
                    setDeletePending(false)
                  }
                }}
              >
                {t('delete')}
              </Button>
            </>
          }
        >
          {t('deleteSessionWithFilesDescription')}
        </Modal>
      ) : null}
    </div>
  )
}

/** 移动端抽屉：浮在页面上方，点空白区域关闭。 */
export const MobileSider = () => {
  const open = useMobileSider((state) => state.open)
  if (!open) {
    return null
  }
  return (
    <>
      <div className={styles.drawerMask} onClick={closeMobileSider} />
      <div className={styles.drawer}>
        <Sider mobile />
      </div>
    </>
  )
}
