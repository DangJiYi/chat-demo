import { createRoot } from 'react-dom/client'
import { App } from './app/App.js'
import { setupTheme } from './app/theme.js'
import { ensureKatex } from './services/markdown/katex.js'
import './app/theme.css'
import './app/global.css'
import { getKeyStatus, SERVER_KEY } from './local/api.js'
import { setApiKey, useSettings } from './services/settings.js'

ensureKatex()
setupTheme()

// 服务端已经存了 Key 时（比如上一次运行保存的），前端补上哨兵值，否则会误判成"未配置"。
void getKeyStatus().then((status) => {
  if (!status.configured) {
    // 服务端没有 Key：前端残留的哨兵要清掉，否则请求会被上游 401
    if (useSettings.getState().apiKey === SERVER_KEY) {
      setApiKey(null)
    }
    return
  }
  if (useSettings.getState().apiKey !== SERVER_KEY) {
    setApiKey(SERVER_KEY)
  }
}).catch(() => {})

createRoot(document.getElementById('root') as HTMLElement).render(<App />)

// 挂载成功就把首屏占位去掉
document.getElementById('boot-fallback')?.remove()
