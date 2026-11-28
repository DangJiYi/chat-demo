/**
 * 录音并编码为 ASR 接受的 WAV（单声道、16-bit PCM、16kHz）。
 *
 * 采样链路：getUserMedia → createMediaStreamSource → analyser → ScriptProcessor。
 * 录音处理在页面内完成，不依赖 MediaRecorder（安卓 WebView 对它支持不稳定）。
 *
 * **采样率不能硬编码成 16k。** 安卓 WebView 的 AudioContext 只能跑硬件采样率
 * （通常 44.1k/48k），`new AudioContext({ sampleRate: 16000 })` 会抛
 * NotSupportedError。那个异常会让录音按钮“闪一下就没了”——状态立刻回 idle，
 * 用户根本没机会说话。所以这里：先试 16k，失败就退回默认采样率，
 * 在 encodeWav 里重采样到 16k（ASR 要的格式）。
 */
export const SAMPLE_RATE = 16000
/** ASR 单次时长上限 120 秒 */
export const MAX_SECONDS = 120

const BUFFER_SIZE = 4096

export type Recording = {
  /** 停止录音并返回 WAV；没有采到声音时返回 null */
  stop: () => Promise<Blob | null>
  /** 放弃录音，释放麦克风 */
  cancel: () => void
  /** 频谱分析节点，只读；采样率与录音一致 */
  analyser: AnalyserNode
}

/**
 * 建 AudioContext。
 *
 * 优先用 16k：能省掉重采样，也最贴近上游期望。
 * 安卓上这个请求基本都会失败（硬件只支持 44.1k/48k），于是回退到默认采样率。
 * 两条路径都返回可用的 context；真正的采样率一律从 `context.sampleRate` 读，
 * 不信我们请求的那个值——有些浏览器不抛错，而是静默给一个不同的采样率。
 */
const createContext = (): AudioContext => {
  try {
    return new AudioContext({ sampleRate: SAMPLE_RATE })
  } catch {
    return new AudioContext()
  }
}

/**
 * 打开麦克风，失败时重试一次。
 *
 * `NotReadableError`（Could not start audio source）在安卓上很常见的原因是
 * 上一路录音刚被 stop，设备还没真正释放；等一小下再开就好了。
 * 如果是其他应用在占麦克风，重试也会失败，所以只重试一次、只针对这一个错误，
 * 避免把“真的用不了”拖成慢失败。
 */
const openMicrophone = async (): Promise<MediaStream> => {
  const constraints: MediaStreamConstraints = {
    audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true },
  }
  try {
    return await navigator.mediaDevices.getUserMedia(constraints)
  } catch (error) {
    const notReadable = error instanceof Error && error.name === 'NotReadableError'
    if (!notReadable) {
      throw error
    }
    await new Promise((resolve) => setTimeout(resolve, 400))
    return navigator.mediaDevices.getUserMedia(constraints)
  }
}

/** 开始录音；到达时长上限时调用 onLimit，由调用方决定何时 stop */
export const startRecording = async (onLimit: () => void): Promise<Recording> => {
  const stream = await openMicrophone()
  let context: AudioContext | undefined
  let source: MediaStreamAudioSourceNode | undefined
  let processor: ScriptProcessorNode | undefined
  let analyser: AnalyserNode | undefined
  let released = false

  const release = () => {
    if (released) {
      return
    }
    released = true
    if (processor) {
      processor.onaudioprocess = null
    }
    // 初始化可能只完成了一部分；单个节点断开失败也不能阻止麦克风释放。
    for (const node of [processor, analyser, source]) {
      try {
        node?.disconnect()
      } catch {
        // 继续释放其余资源。
      }
    }
    for (const track of stream.getTracks()) {
      track.stop()
    }
    void context?.close().catch(() => {})
  }

  try {
    context = createContext()
    const inputRate = context.sampleRate
    // 预分配整段上限的缓冲：边采边写，省掉一堆小 Float32Array 和它们的 GC。
    // 48k 下 120 秒约 23 MB，和原来攒 chunks 的总量相当，但峰值更可控。
    const capacity = Math.ceil(MAX_SECONDS * inputRate)
    const buffer = new Float32Array(capacity)
    let filled = 0
    let limited = false

    source = context.createMediaStreamSource(stream)
    processor = context.createScriptProcessor(BUFFER_SIZE, 1, 1)
    // 频谱分析用：串在录音链路中间（原样透传），WebKit 不会处理未连到输出的分析节点
    analyser = context.createAnalyser()
    analyser.fftSize = 512
    analyser.smoothingTimeConstant = 0.2

    processor.onaudioprocess = (event) => {
      if (limited) {
        return
      }
      const data = event.inputBuffer.getChannelData(0)
      const room = capacity - filled
      if (room <= 0) {
        limited = true
        onLimit()
        return
      }
      // 输入块比剩余空间大时只取前一段，避免越界写入
      const take = Math.min(room, data.length)
      buffer.set(take === data.length ? data : data.subarray(0, take), filled)
      filled += take
      if (filled >= capacity) {
        limited = true
        onLimit()
      }
    }
    source.connect(analyser)
    analyser.connect(processor)
    // ScriptProcessorNode 只有连到输出才会持续回调；输出缓冲保持静音
    processor.connect(context.destination)
    // 在 await getUserMedia 之后创建，iOS 上可能处于 suspended
    if (context.state === 'suspended') {
      void context.resume().catch(() => {})
    }

    return {
      stop: async () => {
        const rate = inputRate
        const taken = filled
        release()
        return taken === 0 ? null : encodeWav(buffer.subarray(0, taken), rate)
      },
      cancel: release,
      analyser,
    }
  } catch (error) {
    release()
    throw error
  }
}

/**
 * 盒式滤波降采样（线性平均）。
 *
 * 用平均而不是直接抽取：直接每 N 个取一个会把高频折回来变成噪声
 * （48k → 16k 时尤其明显），平均相当于一个粗糙的低通，语音识别更准。
 * 任意比例都成立，不要求整除。
 */
const resample = (input: Float32Array, from: number, to: number): Float32Array => {
  if (from === to) {
    return input
  }
  const ratio = from / to
  const outLength = Math.max(1, Math.floor(input.length / ratio))
  const out = new Float32Array(outLength)
  for (let i = 0; i < outLength; i++) {
    const start = Math.floor(i * ratio)
    const end = Math.min(input.length, Math.floor((i + 1) * ratio))
    let sum = 0
    let count = 0
    for (let j = start; j < end; j++) {
      sum += input[j]
      count++
    }
    out[i] = count > 0 ? sum / count : 0
  }
  return out
}

/** 把采集到的浮点样本编码成 16k 单声道 16-bit PCM 的 WAV。 */
const encodeWav = (input: Float32Array, inputRate: number) => {
  const samples = resample(input, inputRate, SAMPLE_RATE)
  const buffer = new ArrayBuffer(44 + samples.length * 2)
  const view = new DataView(buffer)
  const writeString = (offset: number, text: string) => {
    for (let i = 0; i < text.length; i++) {
      view.setUint8(offset + i, text.charCodeAt(i))
    }
  }
  writeString(0, 'RIFF')
  view.setUint32(4, 36 + samples.length * 2, true)
  writeString(8, 'WAVE')
  writeString(12, 'fmt ')
  view.setUint32(16, 16, true)
  view.setUint16(20, 1, true) // PCM
  view.setUint16(22, 1, true) // 单声道
  view.setUint32(24, SAMPLE_RATE, true)
  view.setUint32(28, SAMPLE_RATE * 2, true)
  view.setUint16(32, 2, true)
  view.setUint16(34, 16, true)
  writeString(36, 'data')
  view.setUint32(40, samples.length * 2, true)
  let offset = 44
  for (let i = 0; i < samples.length; i++) {
    const sample = Math.max(-1, Math.min(1, samples[i]))
    view.setInt16(offset, sample < 0 ? sample * 0x8000 : sample * 0x7fff, true)
    offset += 2
  }
  return new Blob([buffer], { type: 'audio/wav' })
}
