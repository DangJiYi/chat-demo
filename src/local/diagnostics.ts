/**
 * 运行日志上报：把前端看到的失败送到服务端，落进 `data/app.log`，
 * 界面上的诊断面板能直接读到。
 *
 * 为什么需要它：这个项目里最耗时间的一类 bug 共同点是**不报错**——
 * 点下去没反应、界面显示错但控制台干净。用户只能描述“不行”，
 * 开发者只能猜。把失败统一送到一个能看见的地方，这类问题就从
 * “猜”变成“看”。
 *
 * 三条刻意的设计：
 *   1. **用裸 fetch，不走 api.ts 的封装**。封装会抛错、会重试、会解析 JSON，
 *      而“记录一条错误”本身绝不能再失败一次——否则错误处理里报错会递归。
 *   2. **自己 catch 掉所有异常**。上报失败就放弃，不影响任何业务逻辑。
 *   3. **keepalive**。页面关闭/跳转时请求也能发出去。
 */

/** 服务端接收上报的路径（见 rust/src/http.rs 的 /local/log）。 */
const LOG_PATH = '/local/log'

/**
 * 上报一条错误。
 *
 * @param where 出问题的位置，例如 `POST /local/sync/now` 或 `window.onerror`
 * @param detail 具体信息（状态码、异常消息、堆栈首行）
 */
export const reportError = (where: string, detail: string): void => {
  try {
    const line = `${where} -> ${detail}`.slice(0, 600)
    void fetch(LOG_PATH, {
      method: 'POST',
      headers: { 'Content-Type': 'text/plain; charset=utf-8' },
      body: line,
      keepalive: true,
    }).catch(() => {})
  } catch {
    // 连 fetch 都不可用（极早期、被 CSP 拦），放弃。
  }
}

/**
 * 包一层 fetch：非 2xx 时上报，但仍把响应原样返回给调用方。
 *
 * `quiet` 用于轮询：服务没起来时每 5 秒失败一次，不传 quiet 会把日志刷满，
 * 把真正有用的一条淹掉。
 */
export const fetchReported = async (
  url: string,
  init?: RequestInit,
  quiet = false
): Promise<Response> => {
  const method = (init?.method ?? 'GET').toUpperCase()
  try {
    const res = await fetch(url, init)
    if (!res.ok && !quiet) {
      reportError(`${method} ${url}`, `HTTP ${res.status}`)
    }
    return res
  } catch (e) {
    if (!quiet) {
      reportError(`${method} ${url}`, e instanceof Error ? e.message : String(e))
    }
    throw e
  }
}

/** 装上全局钩子：未捕获异常与未处理的 Promise 拒绝。 */
export const installDiagnostics = (): void => {
  window.addEventListener('error', (ev) => {
    const where = `${ev.filename || '?'}:${ev.lineno || 0}`
    reportError('window.onerror', `${ev.message} @${where}`)
  })
  window.addEventListener('unhandledrejection', (ev) => {
    const r: unknown = ev.reason
    const detail =
      r instanceof Error ? `${r.name}: ${r.message}` : typeof r === 'string' ? r : String(r)
    reportError('unhandledrejection', detail)
  })
}
