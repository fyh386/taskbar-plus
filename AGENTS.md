# AGENTS.md

## 项目概况

TaskBar Plus：可插拔 Windows 任务栏插件工具。Tauri 2（Rust + WebView2）+ 无构建前端。
目标：低系统占用、高可用、插件扩展无瓶颈。

## 目录结构

- `ui/` — 前端（index.html / dock.js / settings.js / style.css），原生 JS，无打包步骤
- `plugins/<name>/` — 内置插件：每个插件 = `plugin.json` + `index.html`（单文件，内联 CSS/JS），编译期 `include_dir!` 嵌入二进制
- `src-tauri/src/` — Rust 后端：main.rs（命令网关）、stats.rs、git.rs、plugins.rs、config.rs、anchor.rs
- `design/mockup.html` — 设计效果图（纯展示）
- 用户数据：`%APPDATA%\com.taskbarplus.app\settings.json` + `plugins/`

## 构建与运行

```bash
npm install
npm run dev     # cargo tauri dev
npm run build   # cargo tauri build → src-tauri/target/release/taskbar-plus.exe
npm test        # cargo test + tests/contract.test.js + tests/smoke.ps1
```

## 测试约定

- Rust 测试在各模块 `#[cfg(test)]` 中；并行测试的临时目录必须唯一
  （pid + 净化线程名 + 时间戳），否则同目录竞争失败
- `tests/contract.test.js`（node，无依赖）校验插件协议：manifest 必填字段、
  width 约束、API↔权限交叉检查、必须调用 `tbp.ready()`、UTF-8 严格解码
- `tests/smoke.ps1` 为端到端冒烟，会启动并结束 release 进程，不要与 dev 同时跑
- ⚠️ `.ps1` 文件必须保存为 **UTF-8 带 BOM**，否则 PowerShell 5.1 按 ANSI 解析中文报错

## 关键约定

1. **文件编码**：所有源文件必须保持 UTF-8（无 BOM）。⚠️ 不要用 PowerShell 5.1 的
   `Set-Content`/`Get-Content | Set-Content` 处理任何文本文件——默认 ANSI 编码会转码破坏
   中文注释与 HTML（曾导致 DOM 损坏、Rust 大括号被注释吞掉）。编辑文件用 edit/write 工具。
2. **插件协议**：插件通过 `window.tbp` 桥调用宿主（见 README「插件 JS API」）。
   `tbp.on()` 必须在 iframe 本地注册 + 通知父窗口路由表（`TBP_HOST.on`）两段式，缺一不可。
   插件渲染完成后必须调 `tbp.ready()`，宿主依赖它测量真实宽度并上报窗口尺寸。
3. **事件流**：Rust 只 emit 到 label 为 `dock` 的窗口；设置变更后 emit
   `plugins-changed` / `settings-changed`，dock 自动重载。
4. **状态管理**：`AppState` 在 `main()` 中、`Builder.manage()` 之前创建——配置窗口
   （tauri.conf.json 预声明、隐藏）在 setup 完成前就会加载页面并调用命令。
5. **锚定**：`anchor.rs` 每 2s 检测任务栏/托盘矩形，差异更新位置与尺寸，避免
   SetWindowPos 频繁调用导致层级抖动。
6. **监控采集**：`stats.rs` 仅在有订阅者时采样（1s 间隔），用 `tokio::time::sleep`，
   不要用 `spawn_blocking` 睡眠（实测会挂起）。
7. **CSP 必须为 null**：Tauri 在 release 会给 CSP 自动注入 nonce，一旦有 nonce
   `'unsafe-inline'` 即失效，而插件是运行时 srcdoc 注入的（拿不到 nonce），
   脚本会被全部拦截（表现为插件点击无反应、监控无数据）。插件安全由权限网关保证。
8. **release 无控制台**：`#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`
   已设置；前端错误与自检写入 `%APPDATA%\com.taskbarplus.app\tbp.log`（js_log 命令）。

## 已知限制 / TODO

- 全局呼出热键未实现（仅存储配置项）
- 浅色主题未实现
- 插件市场社区条目为占位
- 日志时间为 UTC（未做时区换算，仅用于排查）
- 插件 iframe 使用 `allow-scripts allow-same-origin`（同源可控，第三方插件需注意信任来源）
