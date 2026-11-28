import { throwIfAborted } from './abort.js'

// 上传图片的长边上限，单位为像素。
const MAX_IMAGE_EDGE = 4096

/**
 * 图片无法被平台解码时抛这个。
 *
 * 典型场景是 HEIC：上游接口不接受 image/heic，我们只能转成 JPEG，
 * 而转码依赖浏览器/WebView 自身的 HEIF 解码能力（Android 9+、iOS Safari 有，
 * 桌面 Chrome 没有）。解不了就要明确告诉用户「格式不支持，请转换」，
 * 而不是笼统地报「上传失败」——后者会让人以为重试有用。
 */
export class UnsupportedImageError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'UnsupportedImageError'
  }
}

const HEIF_MIME = /^image\/hei[cf]$/i
const HEIF_EXT = /\.(heic|heif)$/i

/**
 * 是不是 HEIF 系（HEIC/HEIF）。
 *
 * 同时看 MIME 和后缀：安卓 WebView 通过文件选择器拿到 HEIC 时，
 * `file.type` 经常是空串，只有文件名靠得住。
 */
export const isHeifLike = (file: File): boolean =>
  HEIF_MIME.test(file.type) || HEIF_EXT.test(file.name)

const readUploadImage = async (blob: Blob) => {
  const image = new Image()
  const url = URL.createObjectURL(blob)
  try {
    // 通过临时 URL 解码本地文件，从 naturalWidth 和 naturalHeight 读取图片本身的尺寸。
    image.src = url
    await image.decode()
    return { image, longestEdge: Math.max(image.naturalWidth, image.naturalHeight) }
  } finally {
    URL.revokeObjectURL(url)
  }
}

/**
 * 复用旧 file_id 前，用本地原件检查当时上传的图片是否超限。
 *
 * 解不出来时返回 true（当作「需要重新上传」）：宁可多传一次、让
 * resizeUploadImage 去转码，也不要复用一个可能是 HEIC、上游根本收不了的文件。
 */
export const isUploadImageOversized = async (blob: Blob): Promise<boolean> => {
  try {
    return (await readUploadImage(blob)).longestEdge > MAX_IMAGE_EDGE
  } catch {
    return true
  }
}

/**
 * 图片上传前的整理：
 *   - HEIC/HEIF 一律转成 JPEG（上游不接受 HEIC，与尺寸无关）
 *   - 长边超过 4096 的按比例缩小
 *   - 其余原样返回，不重编码（避免无谓的有损压缩）
 */
export const resizeUploadImage = async (file: File, signal?: AbortSignal): Promise<File> => {
  throwIfAborted(signal)
  const heif = isHeifLike(file)

  let image: HTMLImageElement
  let longestEdge: number
  try {
    const read = await readUploadImage(file)
    image = read.image
    longestEdge = read.longestEdge
  } catch {
    // 连尺寸都读不到：要么是 HEIF 而平台解不了，要么文件本身损坏。
    // 两种情况的用户动作一样——换个格式。
    throw new UnsupportedImageError(`Cannot decode image: ${file.name}`)
  }
  throwIfAborted(signal)

  if (!heif && longestEdge <= MAX_IMAGE_EDGE) {
    return file
  }

  // 宽和高乘以同一个比例，横图竖图都按原比例缩到长边上限。
  // HEIC 即使尺寸合格也要走这一趟（转码），所以取 min 保证绝不放大。
  const scale = Math.min(1, MAX_IMAGE_EDGE / longestEdge)
  const canvas = document.createElement('canvas')
  // 画布宽高需要整数像素。极细长图的短边可能舍入为 0，因此至少保留 1 像素。
  canvas.width = Math.max(1, Math.round(image.naturalWidth * scale))
  canvas.height = Math.max(1, Math.round(image.naturalHeight * scale))
  try {
    const context = canvas.getContext('2d')
    if (!context) {
      throw new Error('Cannot resize image')
    }
    // 先铺白底：JPEG 没有透明通道，PNG/HEIC 的透明区域否则会变成黑块。
    context.fillStyle = '#ffffff'
    context.fillRect(0, 0, canvas.width, canvas.height)
    // drawImage 将整张图重采样到目标尺寸的画布上。动图在这一步只绘制一帧。
    context.drawImage(image, 0, 0, canvas.width, canvas.height)

    // 输出格式：HEIC 一律 JPEG；其余沿用原格式（只有 JPEG/WebP 支持质量参数）。
    let type: string
    if (heif) {
      type = 'image/jpeg'
    } else if (file.type === 'image/jpeg' || file.type === 'image/webp') {
      type = file.type
    } else {
      type = 'image/png'
    }
    const quality = type === 'image/png' ? 1 : 0.92
    const blob = await new Promise<Blob>((resolve, reject) => {
      canvas.toBlob(
        (result) => (result ? resolve(result) : reject(new Error('Cannot encode image'))),
        type,
        quality
      )
    })
    throwIfAborted(signal)
    // 浏览器可能回退到 PNG，上传文件的后缀以实际编码为准。
    const extension =
      blob.type === 'image/jpeg' ? 'jpg' : blob.type.slice('image/'.length)
    const name = file.name.replace(/\.[^.]*$/, '') + '.' + extension
    return new File([blob], name, { type: blob.type, lastModified: file.lastModified })
  } finally {
    canvas.width = canvas.height = 0
  }
}
