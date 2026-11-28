import { useCallback, useEffect, useRef, useState } from 'react'
import type { TranslationMessage } from '../../i18n/index.js'
import { transcribe } from '../../services/api/speech.js'
import { startRecording, type Recording } from '../../services/speech/recorder.js'
import { useSettings } from '../../services/settings.js'
import { showToast } from '../base/toastState.js'
import { reportError } from '../../local/diagnostics.js'

export type VoiceInputStatus = 'idle' | 'recording' | 'transcribing'

/**
 * 录音依赖 getUserMedia，只在安全上下文可用（HTTPS、localhost、file://）。
 * 普通 HTTP 页面上直接禁用入口，避免点击后必然失败。
 */
const AVAILABLE = typeof window === 'undefined' || window.isSecureContext

const describeRecordingError = (error: unknown): TranslationMessage => {
  if (error instanceof DOMException) {
    if (error.name === 'NotAllowedError' || error.name === 'SecurityError') {
      return { key: 'voiceInputPermissionDenied' }
    }
    if (error.name === 'NotFoundError') {
      return { key: 'voiceInputNoDevice' }
    }
    // NotReadableError：权限已给、设备也在，但音频源起不来。
    // 单独给它一条文案，否则用户只能看到“失败”，不知道该去关掉什么。
    if (error.name === 'NotReadableError') {
      return { key: 'voiceInputBusy' }
    }
  }
  return { key: 'voiceInputFailed' }
}

/**
 * 语音输入（oneshot）：点击开始录音，再次点击结束并转写，结果交给 onText 填入输入框。
 * 转写中再次点击取消。录满 120 秒自动结束。
 */
export const useVoiceInput = (onText: (text: string) => void) => {
  const [status, setStatus] = useState<VoiceInputStatus>('idle')
  /** 本次录音已录秒数，只在录音中递增 */
  const [seconds, setSeconds] = useState(0)
  const recordingRef = useRef<Recording | null>(null)
  const controllerRef = useRef<AbortController | null>(null)
  const operationRef = useRef(0)
  // 是否有一个 getUserMedia 还在进行中（已点开始、麦克风还没真正打开）。
  //
  // 没有这个标记时，连点两下会同时发起两次 getUserMedia，安卓上第二次
  // 必然失败——同一个麦克风不能被同一页面开两路，报的就是
  // `NotReadableError: Could not start audio source`。
  // 之前日志里连着两条一模一样的错误，正是这个竞态。
  const startingRef = useRef(false)
  const onTextRef = useRef(onText)
  onTextRef.current = onText

  const cancelOperation = useCallback(() => {
    operationRef.current++
    recordingRef.current?.cancel()
    recordingRef.current = null
    controllerRef.current?.abort()
    controllerRef.current = null
    // 这里**不**清 startingRef：getUserMedia 还在飞，清了它就可能再发一路，
    // 又回到“两路麦克风互相挤”的老问题。由 toggle 里 await 的两条分支
    // （成功/失败）负责清。
  }, [])

  const cancel = useCallback(() => {
    cancelOperation()
    setStatus('idle')
  }, [cancelOperation])

  const finish = useCallback(async () => {
    const recording = recordingRef.current
    if (!recording) {
      // 仍在等待麦克风授权时也允许取消；迟到的录音对象会立即释放。
      cancel()
      return
    }
    const operation = operationRef.current
    recordingRef.current = null
    let controller: AbortController | null = null
    try {
      let audio: Blob | null
      try {
        audio = await recording.stop()
      } finally {
        recording.cancel()
      }
      if (operationRef.current !== operation || !audio) {
        return
      }
      controller = new AbortController()
      controllerRef.current = controller
      setStatus('transcribing')
      const text = await transcribe(useSettings.getState().apiKey ?? '', audio, controller.signal)
      if (controller.signal.aborted || operationRef.current !== operation) {
        return
      }
      if (text.trim()) {
        onTextRef.current(text.trim())
      } else {
        showToast({ key: 'voiceInputEmpty' })
      }
    } catch (error) {
      // 转写失败也一样要留痕：否则“录了但没出字”在日志里看不到任何线索。
      if (!controller?.signal.aborted) {
        const name = error instanceof Error ? error.name : typeof error
        const message = error instanceof Error ? error.message : String(error)
        reportError('transcribe', `${name}: ${message}`)
      }
      if (operationRef.current === operation && !controller?.signal.aborted) {
        showToast({ key: 'voiceInputFailed' })
      }
    } finally {
      if (operationRef.current === operation) {
        controllerRef.current = null
        setStatus('idle')
      }
    }
  }, [cancel])

  const toggle = useCallback(async () => {
    if (!AVAILABLE) {
      return
    }
    if (status === 'recording') {
      await finish()
      return
    }
    if (status === 'transcribing') {
      cancel()
      return
    }
    // 麦克风还在打开中：这一下不做任何事，等它落定。
    //
    // 同一页面不能同时开两路麦克风，安卓上第二次会直接报
    // NotReadableError: Could not start audio source。
    // 连点时用户看到的“闪一下就没了”就是这个。
    if (startingRef.current) {
      return
    }
    const operation = ++operationRef.current
    startingRef.current = true
    setStatus('recording')
    try {
      const recording = await startRecording(() => {
        if (operationRef.current === operation) {
          showToast({ key: 'voiceInputLimitReached' })
          void finish()
        }
      })
      startingRef.current = false
      if (operationRef.current !== operation) {
        recording.cancel()
        return
      }
      recordingRef.current = recording
    } catch (error) {
      startingRef.current = false
      // 这里必须上报：录音失败被本函数的 catch 收下，只会弹一个 toast，
      // 永远到不了 window.onerror，所以“点了没反应”时日志里什么都没有。
      // 把 name/message 写进日志，才能区分是权限被拒、没有设备、
      // 还是 AudioContext 采样率不支持——三者的修法完全不同。
      const name = error instanceof Error ? error.name : typeof error
      const message = error instanceof Error ? error.message : String(error)
      reportError('startRecording', `${name}: ${message}`)
      if (operationRef.current === operation) {
        setStatus('idle')
        showToast(describeRecordingError(error))
      }
    }
  }, [status, finish, cancel])

  useEffect(() => {
    if (status !== 'recording') {
      return
    }
    const startedAt = Date.now()
    setSeconds(0)
    // 半秒刷新一次，让显示的秒数与实际录音时长保持一致
    const timer = setInterval(() => {
      setSeconds(Math.floor((Date.now() - startedAt) / 1000))
    }, 500)
    return () => clearInterval(timer)
  }, [status])

  useEffect(() => cancelOperation, [cancelOperation])

  /** 当前录音的频谱分析节点，未录音时为 null */
  const getAnalyser = useCallback(() => recordingRef.current?.analyser ?? null, [])

  return { status, seconds, getAnalyser, toggle, cancel, available: AVAILABLE }
}
