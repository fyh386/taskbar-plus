/**
 * TaskBar Plus — 插件契约测试
 * 校验所有内置插件与用户插件是否符合插件协议：
 *  1. plugin.json 必填字段 / width 约束 / permissions 合法值
 *  2. index.html 存在且包含 tbp.ready() 调用
 *  3. index.html 中所有 tbp.call('api') 调用的权限均在 manifest 声明
 *  4. 无 GBK 乱码（文件可严格按 UTF-8 解码）
 *  5. JSON 配置默认值合法
 *
 * 运行：node tests/contract.test.js
 */
const fs = require('fs');
const path = require('path');

const ROOT = path.join(__dirname, '..');
const PLUGIN_DIRS = [path.join(ROOT, 'plugins')];
const USER_PLUGIN_DIR = path.join(process.env.APPDATA || '', 'com.taskbarplus.app', 'plugins');
if (fs.existsSync(USER_PLUGIN_DIR)) PLUGIN_DIRS.push(USER_PLUGIN_DIR);

// API → 所需权限（与 src-tauri/src/plugins.rs check_permission 保持一致）
const API_PERMISSION = (api) => {
  if (api.startsWith('http.')) return 'network';
  if (api.startsWith('shell.')) return 'shell';
  if (api.startsWith('git.')) return 'git';
  if (api.startsWith('clipboard.')) return 'clipboard';
  if (api.startsWith('session.')) return 'session';
  if (api.startsWith('fs.')) return 'fs';
  if (api.startsWith('mail.')) return 'network';
  if (api.startsWith('proxy.')) return 'proxy';
  if (api.startsWith('hosts.')) return 'hosts';
  if (api.startsWith('net.')) return 'net';
  if (api.startsWith('docker.')) return 'docker';
  return null; // stats.*/config.*/dock.* 无需权限
};
const KNOWN_APIS = new Set([
  'stats.subscribe', 'stats.unsubscribe', 'stats.clean_memory',
  'translate.text',
  'config.get', 'config.set',
  'http.get', 'http.post', 'http.request',
  'shell.open',
  'git.status', 'git.branches', 'git.switch', 'git.pull', 'git.push',
  'git.log', 'git.conflicts', 'git.outgoing', 'git.last_pull_stat',
  'git.abort', 'git.continue',
  'clipboard.history', 'clipboard.set', 'clipboard.clear',
  'dock.expand', 'dock.collapse',
  'mail.unread',
  'session.write',
  'fs.read', 'fs.write', 'fs.append', 'fs.save_dialog', 'fs.write_base64',
  'proxy.start', 'proxy.stop', 'proxy.status',
  'net.ports', 'net.kill',
  'docker.list', 'docker.restart', 'docker.logs',
  'hosts.read', 'hosts.apply',
]);
const VALID_PERMISSIONS = new Set(['network', 'shell', 'git', 'clipboard', 'session', 'fs', 'proxy', 'hosts', 'net', 'docker']);

let passed = 0, failed = 0;
const failures = [];

function check(cond, msg) {
  if (cond) { passed++; }
  else { failed++; failures.push(msg); console.error('  ✗ ' + msg); }
}
function ok(msg) { passed++; }

function scanPlugins(dir, builtin) {
  if (!fs.existsSync(dir)) return;
  for (const name of fs.readdirSync(dir)) {
    const pdir = path.join(dir, name);
    if (!fs.statSync(pdir).isDirectory()) continue;
    const tag = `${builtin ? '内置' : '用户'}插件 ${name}`;
    const mfPath = path.join(pdir, 'plugin.json');
    const htmlPath = path.join(pdir, 'index.html');

    // ---- 文件存在与编码 ----
    check(fs.existsSync(mfPath), `${tag}: 缺少 plugin.json`);
    check(fs.existsSync(htmlPath), `${tag}: 缺少 index.html`);
    if (!fs.existsSync(mfPath) || !fs.existsSync(htmlPath)) continue;

    let mfRaw, html;
    try { mfRaw = fs.readFileSync(mfPath, 'utf8'); ok(); }
    catch (e) { check(false, `${tag}: plugin.json 非 UTF-8 (${e.message})`); continue; }
    try { html = fs.readFileSync(htmlPath, 'utf8'); ok(); }
    catch (e) { check(false, `${tag}: index.html 非 UTF-8 (${e.message})`); continue; }

    // ---- manifest 解析 ----
    let mf;
    try { mf = JSON.parse(mfRaw); ok(); }
    catch (e) { check(false, `${tag}: plugin.json 非法 JSON: ${e.message}`); continue; }

    check(typeof mf.id === 'string' && mf.id.length > 0, `${tag}: id 缺失或为空`);
    check(typeof mf.name === 'string' && mf.name.length > 0, `${tag}: name 缺失`);
    check(typeof mf.version === 'string' && /^\d+\.\d+\.\d+/.test(mf.version), `${tag}: version 非法: ${mf.version}`);
    check(mf.config === undefined || (typeof mf.config === 'object' && mf.config !== null), `${tag}: config 必须是对象`);

    // width 约束
    const w = mf.width || {};
    const mn = w.min ?? 40, mx = w.max ?? 400, df = w.default ?? 160;
    check(Number.isInteger(mn) && Number.isInteger(mx) && Number.isInteger(df), `${tag}: width 必须是整数`);
    check(mn >= 20 && mn <= mx, `${tag}: width.min(${mn}) 应在 [20, max]`);
    check(df >= mn && df <= mx, `${tag}: width.default(${df}) 应在 [min, max] 内`);
    check(mx <= 1200, `${tag}: width.max(${mx}) 过大（>1200）`);

    // permissions 合法值
    const perms = new Set(mf.permissions || []);
    for (const p of perms) check(VALID_PERMISSIONS.has(p), `${tag}: 未知权限 "${p}"`);

    // ---- index.html 契约 ----
    check(/<\/html>\s*$/i.test(html.trim()), `${tag}: index.html 结构不完整（缺 </html>）`);
    check(html.includes('tbp.ready('), `${tag}: 未调用 tbp.ready()（宿主无法测量宽度）`);
    check(/<body/i.test(html), `${tag}: 缺少 <body>`);

    // 禁止直接访问 parent 内部（必须走 tbp 桥）
    const parentDirect = html.match(/parent\.__TAURI__|parent\.TBP_HOST\.call/g);
    check(!parentDirect, `${tag}: 绕过 tbp 桥直接访问宿主内部: ${parentDirect && parentDirect[0]}`);

    // ---- API 权限交叉校验 ----
    const calls = [...html.matchAll(/tbp\.call\(\s*['"`]([\w.]+)['"`]/g)].map(m => m[1]);
    for (const api of new Set(calls)) {
      check(KNOWN_APIS.has(api), `${tag}: 调用了未知 API "${api}"（宿主不支持）`);
      const need = API_PERMISSION(api);
      if (need) check(perms.has(need), `${tag}: 调用 ${api} 但未声明 "${need}" 权限`);
    }
  }
}

console.log('== TaskBar Plus 插件契约测试 ==\n');
for (const dir of PLUGIN_DIRS) scanPlugins(dir, dir.endsWith('plugins') && !dir.includes('AppData'));

// ---- 内置插件唯一性 ----
const builtinDir = path.join(ROOT, 'plugins');
const ids = fs.readdirSync(builtinDir)
  .map(n => {
    try { return JSON.parse(fs.readFileSync(path.join(builtinDir, n, 'plugin.json'), 'utf8')).id || n; }
    catch { return n; }
  });
check(new Set(ids).size === ids.length, `内置插件 id 重复: ${ids}`);

// ---- ui 前端 UTF-8 校验 ----
for (const f of ['index.html', 'dock.js', 'settings.js', 'style.css']) {
  const fp = path.join(ROOT, 'ui', f);
  try { fs.readFileSync(fp, 'utf8'); ok(); }
  catch (e) { check(false, `ui/${f} 非 UTF-8: ${e.message}`); }
  const content = fs.readFileSync(fp, 'utf8');
  check(!content.includes('�'), `ui/${f} 含替换字符（可能曾被错误转码）`);
}

console.log(`\n结果: ${passed} 通过, ${failed} 失败`);
if (failed > 0) { console.log('\n失败项:'); failures.forEach(f => console.log('  - ' + f)); process.exit(1); }
console.log('✓ 全部契约测试通过');
