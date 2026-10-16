# TaskBar Plus

可插拔的 Windows 任务栏插件工具。在任务栏上挂一条可定制的 Dock，按需安装插件：系统监控、剪贴板历史、端口速查、抓包代理、Git 助手……全部即点即用。

- **技术栈**：Tauri 2（Rust + WebView2）+ 无构建原生前端（无打包步骤，低系统占用）
- **插件化**：插件 = `plugin.json` + `index.html` 单文件，宿主以 iframe（srcdoc）加载，通过 `window.tbp` 桥调用宿主能力；市场可在线安装 / 更新 / 一键卸载，无需重新编译主程序
- **权限网关**：每个插件声明所需权限（network / shell / fs / clipboard / hosts / net / docker / git / proxy …），主程序预置常见权限，插件调用逐条校验

## 界面预览

以下截图为各内置插件的弹出面板（示例数据渲染）：

### 系统监控
CPU / 内存 / 网络迷你曲线，点击展开详情与一键清理内存。
![系统监控](docs/screenshots/monitor.png)

### 剪贴板历史
自动记录文本复制历史，点击即可重新复制。
![剪贴板历史](docs/screenshots/clipboard.png)

### 快捷输入
回车一键翻译，Ctrl+回车问 AI，支持多会话历史。
![快捷输入](docs/screenshots/quick-input.png)

### 端口速查
输入端口号立刻看到是谁占用的，一键结束进程；附全部 LISTEN 端口表。
![端口速查](docs/screenshots/port-killer.png)

### 抓包代理
本地 MITM 系统代理抓包，HTTPS 解密，支持筛选与详情查看。
![抓包代理](docs/screenshots/proxy-capture.png)

### Hosts 切换
Hosts 编辑与多套配置方案切换，保存即应用（提权写入 + 自动刷新 DNS）。
![Hosts 切换](docs/screenshots/hosts-switch.png)

### Git 助手
任务栏查看分支，一键 Pull / Push / 切换分支。
![Git 助手](docs/screenshots/git.png)

### Docker 面板
容器状态灯一览，一键重启、查看日志（走 Docker Desktop 命名管道）。
![Docker 面板](docs/screenshots/docker-panel.png)

### HTTP 请求
类 Postman 的轻量 HTTP 调试：方法 / 请求头 / 请求体 / 响应查看。
![HTTP 请求](docs/screenshots/http-req.png)

### 保活请求
定时发起 HTTP 请求，保活 VPN / 代理连接，防止长时间无流量断连。
![保活请求](docs/screenshots/keep-alive.png)

### 快捷启动
自定义应用 / 网站 / 文件夹快捷图标，点击即开。
![快捷启动](docs/screenshots/launcher.png)

### 邮件提醒
IMAP 未读邮件提醒：QQ / 163 / Gmail 授权码登录，定时轮询未读数。
![邮件提醒](docs/screenshots/mail-alert.png)

### 文件搜索
基于 Everything 的本地文件秒搜（需本地安装 Everything 并开启 HTTP 服务器）。
![文件搜索](docs/screenshots/everything-search.png)

### RSS 订阅
RSS / Atom 订阅聚合：轮询抓取，条上滚动头条，面板浏览全部。
![RSS 订阅](docs/screenshots/rss-reader.png)

### 股票盯盘
自选股实时行情轮询，涨红跌绿，点击查看详情。
![股票盯盘](docs/screenshots/stock-watch.png)

### 天气
当前城市天气（wttr.in）。
![天气](docs/screenshots/weather.png)

### 网盘 WebDAV
WebDAV 网盘管理：浏览目录、拖拽上传、下载与整理。
![网盘 WebDAV](docs/screenshots/webdav.png)

### webtool 工具箱
开发者常用工具：JSON 格式化、URL / Base64 编解码、时间戳、哈希、随机密码等。
![webtool 工具箱](docs/screenshots/webtool.png)

## 构建与运行

```bash
npm install
npm run dev     # cargo tauri dev
npm run build   # cargo tauri build → src-tauri/target/release/taskbar-plus.exe
npm test        # cargo test + tests/contract.test.js + tests/smoke.ps1
```

## 插件开发

一个插件就是一个目录：`plugin.json`（清单）+ `index.html`（单文件，内联 CSS/JS），可选 `config` 字段声明可配置项。

```json
{
  "id": "my-plugin",
  "name": "我的插件",
  "version": "1.0.0",
  "description": "一句话说明",
  "width": { "min": 40, "max": 400, "default": 120 },
  "permissions": ["network"],
  "config": { "endpoint": "" }
}
```

插件通过 `window.tbp` 桥与宿主交互：

| API | 说明 |
| --- | --- |
| `tbp.ready()` | 渲染完成后必须调用，宿主据此测量真实宽度 |
| `tbp.call(api, args)` | 调用宿主能力，如 `clipboard.history`、`net.ports`、`stats.subscribe`、`dock.expand` 等 |
| `tbp.on(event, cb)` | 订阅宿主事件（如 `stats`、`dock.collapsed`），iframe 内注册 + 父窗口路由两段式 |
| `tbp.config()` / `tbp.saveConfig()` | 读写插件配置 |

插件协议约束（`tbp.on()` 两段式注册、必须调用 `tbp.ready()`、UTF-8 严格解码等）由 `tests/contract.test.js` 统一校验。

## 插件市场

主程序内置插件市场，从 GitHub 仓库加载 `market.json` 自动发现插件，支持搜索、在线安装、更新与一键卸载。市场仓库示例见 [fyh386/plugin-market](https://github.com/fyh386/plugin-market)。

## 目录结构

```
ui/                 前端（dock.js / settings.js，原生 JS，无打包）
plugins/<name>/     内置插件（plugin.json + index.html，编译期 include_dir! 嵌入）
src-tauri/src/      Rust 后端：main.rs / stats.rs / git.rs / plugins.rs / config.rs / anchor.rs
tests/              协议契约测试、冒烟脚本、截图流水线
release/            预编译 exe
```
