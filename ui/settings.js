/* TaskBar Plus — 设置中心 */
(function () {
  const T = window.__TAURI__;
  const $ = (sel) => document.querySelector(sel);
  const invoke = (cmd, args) => T.core.invoke(cmd, args);

  let plugins = [];
  let settings = null;
  let jsonTarget = null;

  function toast(msg) {
    const t = document.createElement('div');
    t.className = 'toast'; t.textContent = msg;
    document.body.appendChild(t);
    setTimeout(() => t.remove(), 2400);
  }

  function notifyDock() { /* dock 监听 plugins-changed / settings-changed 事件自动刷新 */ }

  const ICO = {
    monitor: '<svg viewBox="0 0 24 24"><path d="M3 12h4l2-6 4 12 2-6h6"/></svg>',
    input: '<svg viewBox="0 0 24 24"><rect x="2" y="6" width="20" height="12" rx="2"/><path d="M6 10h.01M10 10h.01M7 14h10"/></svg>',
    git: '<svg viewBox="0 0 24 24"><circle cx="6" cy="6" r="2.5"/><circle cx="6" cy="18" r="2.5"/><circle cx="18" cy="8" r="2.5"/><path d="M6 8.5v7M6 8.5c0 4 4.5 3 6 5.5 1 1.8.5 4-1 5.5"/></svg>',
    weather: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/></svg>',
    plugin: '<svg viewBox="0 0 24 24"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg>',
  };
  function iconFor(p) {
    if (p.id.includes('monitor')) return ICO.monitor;
    if (p.id.includes('input')) return ICO.input;
    if (p.id.includes('git')) return ICO.git;
    if (p.id.includes('weather')) return ICO.weather;
    return ICO.plugin;
  }

  // ==================== 插件管理 ====================

  async function renderManage() {
    plugins = await invoke('plugins_list');
    const page = $('#page-manage');
    page.innerHTML = `<div class="sec-title">插件管理</div>
      <div class="sec-desc">共 ${plugins.length} 个插件 · 启用/禁用、调整长度限制、编辑 JSON 配置</div>`;
    if (!plugins.length) {
      page.innerHTML += '<div class="empty-hint">还没有安装任何插件</div>';
      return;
    }
    for (const p of plugins) {
      const card = document.createElement('div');
      card.className = 'mgr-card';
      card.innerHTML = `
        <span class="pc-ico" style="background:rgba(76,141,255,.14);color:#4c8dff">${iconFor(p)}</span>
        <div class="pc-info">
          <div class="pc-name">${esc(p.name)} <span class="pc-ver">v${esc(p.version)}</span>
            ${p.builtin ? '<span class="pc-ver">内置</span>' : '<span class="pc-ver" style="color:#6ee7a0">用户</span>'}</div>
          <div class="pc-author">${esc(p.author)} · ${esc(p.description)}</div>
          <div class="pc-meta">权限 ${(p.permissions || []).map(permLabel).join(' / ') || '无'} · ${fmtSize(p.size)} · 条带 ${p.resolved_width}px</div>
        </div>
        <div class="pc-width">长度
          <input type="range" min="${Math.min(p.width.min, 20)}" max="${p.width.max}" value="${p.resolved_width}">
          <span class="w-val">${p.resolved_width}px</span>
        </div>
        <button class="mini-btn act-css">样式</button>
        <button class="mini-btn act-cfg">配置</button>
        <button class="mini-btn danger act-del">卸载</button>
        <span class="toggle ${p.enabled ? 'on' : ''}"></span>`;
      const range = card.querySelector('input[type=range]');
      const wVal = card.querySelector('.w-val');
      range.onchange = async () => {
        await invoke('plugin_set_width', { id: p.id, width: +range.value });
        wVal.textContent = range.value + 'px';
        toast('已保存');
      };
      range.oninput = () => { wVal.textContent = range.value + 'px'; };
      card.querySelector('.act-cfg').onclick = () => openJson(p);
      card.querySelector('.act-css').onclick = () => openCss(p);
      card.querySelector('.act-del').onclick = async () => {
        if (!confirm(`卸载插件「${p.name}」？`)) return;
        try { await invoke('plugin_uninstall', { id: p.id }); toast('已卸载'); renderManage(); }
        catch (e) { toast('卸载失败：' + e); }
      };
      card.querySelector('.toggle').onclick = async (e) => {
        const on = !e.target.classList.contains('on');
        e.target.classList.toggle('on', on);
        await invoke('plugin_set_enabled', { id: p.id, enabled: on });
        toast(on ? '已启用' : '已禁用');
      };
      page.appendChild(card);
    }
  }

  // ==================== 插件市场 ====================

  let marketList = [];   // 最近一次拉取的市场索引
  let marketLocal = {};  // id -> 本地插件运行时信息（版本/是否内置）

  function fmtSize(n) {
    n = +n || 0;
    if (n >= 1024 * 1024) return (n / 1024 / 1024).toFixed(1) + ' MB';
    if (n >= 1024) return (n / 1024).toFixed(1) + ' KB';
    return n + ' B';
  }

  function permLabel(p) {
    return ({ network: '网络', shell: '命令', git: 'Git', stats: '系统' })[p] || p;
  }

  async function renderMarket() {
    plugins = await invoke('plugins_list');
    marketLocal = {};
    for (const p of plugins) marketLocal[p.id] = p;
    settings = await invoke('settings_get');
    const page = $('#page-market');
    page.innerHTML = `<div class="sec-title">插件市场</div>
      <div class="sec-desc">从 GitHub/Gitee 仓库自动加载全部插件，支持搜索、查看详情、一键安装与更新。
        仓库根目录需放置 market.json 索引，每个插件一个子目录（plugin.json + index.html）。</div>
      <div class="mk-bar">
        <input id="mk-url" placeholder="市场仓库地址，如 owner/repo 或 https://github.com/owner/repo" value="${esc(settings.general.market_url || '')}">
        <button class="mini-btn" id="mk-urlsave">保存</button>
        <button class="btn-primary" id="mk-load">加载插件</button>
      </div>
      <div class="mk-search"><input id="mk-q" placeholder="搜索插件名称 / 描述 / 作者"></div>
      <div class="mk-status" id="mk-status"></div>
      <div class="mk-grid"></div>`;
    const grid = page.querySelector('.mk-grid');
    const status = $('#mk-status');

    const draw = () => {
      const q = ($('#mk-q').value || '').trim().toLowerCase();
      grid.innerHTML = '';
      let shown = 0;
      for (const m of marketList) {
        const text = [m.id, m.name, m.description, m.author].filter(Boolean).join(' ').toLowerCase();
        if (q && !text.includes(q)) continue;
        shown++;
        const local = marketLocal[m.id];
        const updatable = local && local.version !== m.version && !local.builtin;
        const c = document.createElement('div');
        c.className = 'mk-card';
        const shot = (m.screenshots && m.screenshots[0]) || '';
        const perms = (m.permissions || []).map(p => `<span class="mk-perm">${esc(permLabel(p))}</span>`).join('');
        c.innerHTML = `
          ${shot ? `<img class="mk-shot" src="${esc(shot)}" referrerpolicy="no-referrer" onerror="this.remove()">` : ''}
          <div class="mk-head"><span class="pc-ico" style="width:30px;height:30px;background:rgba(76,141,255,.14);color:#4c8dff">${ICO.plugin}</span>
            <div><div class="mk-name">${esc(m.name || m.id)}</div><div class="mk-by">by ${esc(m.author || '未知')}</div></div></div>
          <div class="mk-desc">${esc(m.description || '')}</div>
          <div class="mk-meta">
            <span>v${esc(m.version || '?')}</span>
            ${m.size ? `<span>${fmtSize(m.size)}</span>` : ''}
            ${perms}
            ${m.homepage ? `<a class="mk-home" href="#" data-home="${esc(m.homepage)}">主页</a>` : ''}
          </div>
          <button class="mk-install" data-id="${esc(m.id)}">${
            local ? (updatable ? `更新 v${esc(m.version)}` : (local.builtin ? '内置 · 覆盖安装' : '重新安装')) : '安 装'
          }</button>`;
        const btn = c.querySelector('.mk-install');
        if (local && !updatable) btn.classList.add('installed');
        btn.onclick = async () => {
          btn.disabled = true;
          btn.textContent = '安装中…';
          try {
            const id = await invoke('market_install', { id: m.id });
            toast('已安装：' + id);
            plugins = await invoke('plugins_list');
            marketLocal = {};
            for (const p of plugins) marketLocal[p.id] = p;
            draw();
          } catch (e) {
            toast('安装失败：' + e);
            btn.disabled = false;
            btn.textContent = '重 试';
          }
        };
        const home = c.querySelector('.mk-home');
        if (home) home.onclick = (e) => { e.preventDefault(); invoke('open_url', { url: home.dataset.home }); };
        grid.appendChild(c);
      }
      if (!shown) grid.innerHTML = '<div class="empty-hint">没有匹配的插件</div>';
    };

    $('#mk-q').oninput = draw;
    $('#mk-urlsave').onclick = async () => {
      await invoke('settings_set', { patch: { market_url: $('#mk-url').value.trim() } });
      toast('市场地址已保存');
    };
    $('#mk-load').onclick = async () => {
      await invoke('settings_set', { patch: { market_url: $('#mk-url').value.trim() } });
      status.textContent = '正在从仓库加载索引…';
      marketList = [];
      draw();
      try {
        const list = await invoke('market_index');
        marketList = Array.isArray(list) ? list : [];
        status.textContent = marketList.length ? `已加载 ${marketList.length} 个插件` : '索引为空';
        draw();
      } catch (e) {
        status.textContent = '加载失败：' + e;
      }
    };
    // 打开市场页自动加载一次
    $('#mk-load').click();
  }


  // ==================== 导入安装 ====================

  async function renderImport() {
    const page = $('#page-import');
    page.innerHTML = `
      <div class="sec-title">导入插件</div>
      <div class="sec-desc">从本地安装第三方插件。插件在沙箱中运行，敏感能力需在 plugin.json 中声明权限。</div>
      <div class="drop-zone" id="dz">
        <div class="dz-ico"><svg viewBox="0 0 24 24"><path d="M12 16V4M7 9l5-5 5 5"/><path d="M4 20h16"/></svg></div>
        <div>点击选择 <b>插件文件夹</b>（包含 plugin.json 与 index.html）</div>
        <div style="margin-top:6px;font-size:11px;color:#4a4b55">或</div>
        <div style="margin-top:6px">点击选择 <b>.zip 插件包</b></div>
      </div>
      <div class="sec-desc" style="margin-bottom:8px">插件清单示例（plugin.json）：</div>
      <div class="code-block">{
  "id": "com.example.my-plugin",
  "name": "我的插件",
  "version": "1.0.0",
  "author": "you",
  "description": "一句话描述",
  "permissions": ["network"],  // network | shell | git
  "width": { "min": 60, "max": 300, "default": 120 },
  "config": { "key": "默认值" }
}</div>`;
    $('#dz').onclick = async () => {
      const kind = confirm('确定 = 选择 .zip 插件包\n取消 = 选择插件文件夹') ? 'zip' : 'dir';
      try {
        const id = await invoke(kind === 'zip' ? 'plugin_import_zip_dialog' : 'plugin_import_dir_dialog');
        toast('安装成功：' + id);
      } catch (e) {
        if (String(e) && !String(e).includes('取消')) toast('安装失败：' + e);
      }
    };
  }

  // ==================== 通用设置 ====================

  async function renderGeneral() {
    settings = await invoke('settings_get');
    const g = settings.general;
    const page = $('#page-general');
    page.innerHTML = `
      <div class="sec-title">通用设置</div>
      <div class="sec-desc">插件停靠区与全局行为，修改自动保存</div>
      <div class="set-row"><span class="sr-label">停靠位置<small>插件条在任务栏上的位置</small></span>
        <select id="g-position">
          <option value="right">任务栏右侧（托盘前）</option>
          <option value="left">任务栏左侧</option>
        </select></div>
      <div class="set-row"><span class="sr-label">插件总长度限制<small>超出后窗口不再变宽，按住插件条左右拖拽查看隐藏插件</small></span>
        <input type="range" id="g-maxw" min="200" max="1400" step="20" value="${g.max_width}">
        <span style="font-size:11px;color:#9a9ba6;margin-left:8px" id="g-maxw-v">${g.max_width}px</span></div>
      <div class="set-row"><span class="sr-label">插件间距</span>
        <input type="range" id="g-spacing" min="0" max="16" value="${g.spacing}"></div>
      <div class="set-row"><span class="sr-label">主题皮肤<small>一键切换整套配色；选「自定义」可配合下方全局样式自行设计</small></span>
        <select id="g-theme">
          <option value="default">默认深色</option>
          <option value="glass">毛玻璃</option>
          <option value="light">浅色</option>
          <option value="contrast">高对比</option>
          <option value="prism">棱镜</option>
          <option value="nebula">星云</option>
          <option value="gradient">渐变</option>
          <option value="fluid">流体</option>
        </select></div>
      <div class="set-row"><span class="sr-label">背景不透明度</span>
        <input type="range" id="g-opacity" min="40" max="100" value="${g.opacity}"></div>
      <div class="set-row"><span class="sr-label">背景颜色<small>留空使用默认深色；不透明度拉到 0 即为全透明，适配任意桌面底色</small></span>
        <input type="color" id="g-bgcolor" value="${g.bg_color || '#1f2025'}">
        <input type="range" id="g-bgopacity" min="0" max="100" value="${g.bg_opacity != null ? g.bg_opacity : 92}" style="width:110px">
        <span style="font-size:11px;color:#9a9ba6;margin-left:8px" id="g-bgopacity-v">${g.bg_opacity != null ? g.bg_opacity : 92}%</span>
        <button class="mini-btn" id="g-bgclear" style="margin-left:8px">恢复默认</button></div>
      <div class="set-row"><span class="sr-label">背景图片<small>覆盖整条 dock 背景，随条带圆角裁剪；留空不显示</small></span>
        <div class="bg-preview" id="g-bgpreview"></div>
        <button class="mini-btn" id="g-bgpick" style="margin-left:8px">选择图片</button>
        <button class="mini-btn" id="g-bgimgclear" style="margin-left:6px">清除</button>
        <input type="file" id="g-bgfile" accept="image/*" hidden></div>
      <div class="set-row"><span class="sr-label">全局自定义样式<small>CSS 同时作用于 dock 条与所有插件，可整体换肤；个别插件还可在插件管理中单独覆盖</small></span>
        <textarea id="g-customcss" class="css-ta" spellcheck="false" placeholder="例如：:root { --dock-bg: rgba(20,22,30,.85); } .plg-card { border-radius: 10px; }"></textarea>
        <button class="mini-btn" id="g-cssclear" style="margin-left:8px">清空</button></div>
      <div class="set-row"><span class="sr-label">开机自启</span><span class="toggle ${g.autostart ? 'on' : ''}" id="g-autostart"></span></div>
      <div class="set-row"><span class="sr-label">呼出快捷输入快捷键<small>（即将支持全局热键）</small></span>
        <input type="text" class="hotkey" id="g-hotkey" value="${esc(g.hotkey_input)}" style="user-select:text"></div>
      <div class="set-row"><span class="sr-label">数据目录<small>配置文件与第三方插件保存位置</small></span>
        <input type="text" readonly style="flex:1.2;user-select:text" onclick="this.select()" value=""></div>`;
    const dataDir = await invoke('get_data_dir');
    page.querySelector('input[readonly]').value = dataDir;

    const save = (patch) => invoke('settings_set', { patch }).then(s => { settings.general = s; });
    $('#g-position').value = g.position;
    $('#g-position').onchange = e => save({ position: e.target.value });
    $('#g-theme').value = g.theme || 'default';
    $('#g-theme').onchange = e => save({ theme: e.target.value }).then(() => toast('皮肤已切换'));
    $('#g-maxw').oninput = e => $('#g-maxw-v').textContent = e.target.value + 'px';
    $('#g-maxw').onchange = e => save({ max_width: +e.target.value });
    $('#g-spacing').onchange = e => save({ spacing: +e.target.value });
    $('#g-opacity').onchange = e => save({ opacity: +e.target.value });
    $('#g-autostart').onclick = async e => {
      const on = !e.target.classList.contains('on');
      e.target.classList.toggle('on', on);
      await save({ autostart: on });
      toast(on ? '已加入开机自启' : '已取消开机自启');
    };
    $('#g-hotkey').onchange = e => save({ hotkey_input: e.target.value });
    // ---- 背景颜色：留空 = 默认深色；不透明度 0 = 全透明 ----
    const bgPreview = () => {
      const c = $('#g-bgcolor').value, a = (+$('#g-bgopacity').value) / 100;
      const n = parseInt(c.slice(1), 16);
      $('#g-bgpreview').style.backgroundColor = `rgba(${n >> 16},${(n >> 8) & 255},${n & 255},${a})`;
    };
    $('#g-bgopacity').oninput = e => $('#g-bgopacity-v').textContent = e.target.value + '%';
    const saveBg = () => save({ bg_color: $('#g-bgcolor').value, bg_opacity: +$('#g-bgopacity').value }).then(bgPreview);
    $('#g-bgcolor').onchange = saveBg;
    $('#g-bgopacity').onchange = saveBg;
    $('#g-bgclear').onclick = async () => {
      await save({ bg_color: '', bg_image: '' });
      const s = await invoke('settings_get');
      settings.general = s.general;
      $('#g-bgcolor').value = '#1f2025';
      $('#g-bgopacity').value = s.general.bg_opacity != null ? s.general.bg_opacity : 92;
      $('#g-bgopacity-v').textContent = $('#g-bgopacity').value + '%';
      $('#g-bgpreview').style.backgroundImage = 'none';
      bgPreview();
      toast('已恢复默认背景');
    };
    // ---- 背景图片：本地选图 → 缩放压缩为 data URL 存配置 ----
    const MAX_BG_W = 1200;
    const setBgImage = async (dataUrl) => {
      await save({ bg_image: dataUrl || '' });
      $('#g-bgpreview').style.backgroundImage = dataUrl ? `url("${dataUrl}")` : 'none';
    };
    $('#g-bgpick').onclick = () => $('#g-bgfile').click();
    $('#g-bgfile').onchange = e => {
      const file = e.target.files && e.target.files[0];
      if (!file) return;
      const rd = new FileReader();
      rd.onload = () => {
        const img = new Image();
        img.onload = async () => {
          const k = Math.min(1, MAX_BG_W / img.width);
          const cv = document.createElement('canvas');
          cv.width = Math.round(img.width * k);
          cv.height = Math.round(img.height * k);
          cv.getContext('2d').drawImage(img, 0, 0, cv.width, cv.height);
          await setBgImage(cv.toDataURL('image/jpeg', 0.85));
          toast('背景图片已保存');
        };
        img.onerror = () => toast('图片读取失败');
        img.src = rd.result;
      };
      rd.readAsDataURL(file);
      e.target.value = '';
    };
    $('#g-bgimgclear').onclick = async () => { await setBgImage(''); toast('已清除背景图片'); };
    if (g.bg_image) $('#g-bgpreview').style.backgroundImage = `url("${g.bg_image}")`;
    // ---- 全局自定义样式：失焦保存 ----
    $('#g-customcss').value = g.custom_css || '';
    $('#g-customcss').onchange = e => save({ custom_css: e.target.value }).then(() => toast('样式已保存'));
    $('#g-cssclear').onclick = async () => {
      $('#g-customcss').value = '';
      await save({ custom_css: '' });
      toast('已清空全局样式');
    };
    bgPreview();
  }

  // ==================== 关于 ====================

  async function renderAbout() {
    const page = $('#page-about');
    page.innerHTML = `
      <div class="about-wrap">
        <div class="about-logo"><svg viewBox="0 0 24 24"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg></div>
        <div class="about-name">TaskBar Plus</div>
        <div class="about-ver">版本 0.1.0 · Tauri 2 + WebView2 · 低占用设计</div>
        <div style="color:#6a6b76;font-size:12px;line-height:2;margin-bottom:18px">
          常驻内存目标 &lt; 40 MB · 插件按需采集、事件推送、零轮询<br>
          插件格式：单文件 HTML + plugin.json，沙箱运行 + 权限声明
        </div>
        <div class="about-grid">
          <div class="set-row"><span class="sr-label">开源协议</span><span style="color:#9a9ba6;font-size:12px">MIT</span></div>
          <div class="set-row"><span class="sr-label">配置文件</span><span style="color:#9a9ba6;font-size:12px;font-family:Consolas,monospace">settings.json</span></div>
        </div>
        <div style="margin-top:20px;display:flex;gap:10px;justify-content:center">
          <button class="btn-primary" id="bt-quit">退出程序</button>
        </div>
      </div>`;
    $('#bt-quit').onclick = () => invoke('quit_app');
  }

  // ==================== JSON 配置编辑器 ====================

  async function openJson(p) {
    jsonTarget = p.id;
    $('#json-title').textContent = p.name + ' · 用户配置';
    $('#json-err').textContent = '';
    const cfg = await invoke('plugin_config_get', { id: p.id });
    $('#json-editor').value = JSON.stringify(cfg, null, 2);
    $('#json-modal').hidden = false;
  }
  $('#json-save').onclick = async () => {
    try {
      const val = JSON.parse($('#json-editor').value);
      await invoke('plugin_config_set', { id: jsonTarget, cfgVal: val });
      $('#json-modal').hidden = true;
      toast('配置已保存');
    } catch (e) {
      $('#json-err').textContent = 'JSON 无效：' + e.message;
    }
  };
  $('#json-cancel').onclick = $('#json-close').onclick = () => { $('#json-modal').hidden = true; };

  // ==================== 插件自定义样式编辑器 ====================

  let cssTarget = null;
  function openCss(p) {
    cssTarget = p.id;
    $('#css-title').textContent = p.name + ' · 自定义样式';
    $('#css-editor').value = p.custom_css || '';
    $('#css-modal').hidden = false;
  }
  $('#css-save').onclick = async () => {
    await invoke('plugin_set_css', { id: cssTarget, css: $('#css-editor').value });
    $('#css-modal').hidden = true;
    const i = plugins.findIndex(x => x.id === cssTarget);
    if (i >= 0) plugins[i].custom_css = $('#css-editor').value;
    toast('样式已保存');
  };
  $('#css-clear').onclick = () => { $('#css-editor').value = ''; };
  $('#css-cancel').onclick = $('#css-close').onclick = () => { $('#css-modal').hidden = true; };

  function esc(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g,
      c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  }

  // ==================== init ====================

  window.TBP_SETTINGS_INIT = async function () {
    const pages = { manage: renderManage, market: renderMarket, import: renderImport, general: renderGeneral, about: renderAbout };
    document.querySelectorAll('.nav-item').forEach(n => n.onclick = () => {
      document.querySelectorAll('.nav-item').forEach(x => x.classList.remove('on'));
      n.classList.add('on');
      document.querySelectorAll('.sw-page').forEach(x => x.classList.remove('on'));
      const pg = document.getElementById('page-' + n.dataset.page);
      pg.classList.add('on');
      pages[n.dataset.page]();
    });
    await renderManage();
  };
})();
