/* TaskBar Plus — dock 窗口：插件加载器 + host 桥接运行时 */
(function () {
  const T = window.__TAURI__;

  // 调试：将错误回传后端日志
  function jslog(m) { try { T.core.invoke('js_log', { msg: m }); } catch (e) {} }
  window.addEventListener('error', (e) => {
    jslog('error: ' + (e.message || e.error) + ' @' + (e.filename || '') + ':' + (e.lineno || ''));
  });
  window.addEventListener('unhandledrejection', (e) => {
    jslog('rejection: ' + (e.reason && e.reason.message || e.reason));
  });

  window.TBP_BOOTSTRAP = async function () {
    let label = 'dock';
    try { label = T.window.getCurrentWindow().label; } catch (e) {}
    if (label === 'settings') {
      document.title = 'tbp-settings-booting';
      document.body.classList.add('settings-mode');
      document.getElementById('settings-root').hidden = false;
      if (window.TBP_SETTINGS_INIT) await window.TBP_SETTINGS_INIT();
      document.title = 'tbp-settings-ready';
      return;
    }
    document.body.classList.add('dock-mode');
    document.getElementById('dock-root').hidden = false;
    await initDock();
  };

  // ==================== host 桥 ====================

  const frames = {};        // pid -> iframe
  const listeners = [];     // {pid, event, cb}
  let expandedPid = null;   // 同时只允许一个插件展开
  const STRIP_H = 40;
  // 弹层额外高度的硬上限（与 Rust 侧 dock_report_size 的 min(1000) 对齐）：
  // 超限部分由插件内部滚动消化，窗口不再跟进，避免弹层顶被裁掉
  const MAX_POPUP_EXTRA = 1000;

  // 持有 stats 订阅的插件：宿主侧生命周期管理——插件 iframe 被销毁（禁用/重载/
  // 卸载）时 Rust 计数无法自行归零，必须在摘除 iframe 的同时退订，否则采样循环
  // 会因为计数残留永远空转（"禁用系统监控后后台仍在实时采样"的根因）
  const statsSubs = new Set();

  // ==================== 展开内容尺寸跟随 ====================
  // 弹层面板内的内容是动态变化的（抓包列表持续进流量、异步渲染等），
  // 若只在展开瞬间量一次高度，内容长高后 body(flex-end + overflow:hidden)
  // 会把顶部（工具栏/上边框）裁掉。这里在展开期间用 ResizeObserver
  // 监听插件文档 body 的直接子元素，变化后防抖复测贴合窗口高度。
  let contentObserver = null;
  let contentObsTimer = 0;

  function watchExpandedContent(f) {
    unwatchExpandedContent();
    try {
      const doc = f && f.contentDocument;
      if (!doc || !doc.body) return;
      contentObserver = new ResizeObserver(() => {
        clearTimeout(contentObsTimer);
        contentObsTimer = setTimeout(() => refitExpanded(f), 120);
      });
      for (const el of doc.body.children) contentObserver.observe(el);
    } catch (e) { contentObserver = null; }
  }

  function unwatchExpandedContent() {
    clearTimeout(contentObsTimer);
    if (contentObserver) {
      try { contentObserver.disconnect(); } catch (e) {}
      contentObserver = null;
    }
  }

  window.TBP_HOST = {
    call(pid, api, args) {
      // dock 尺寸类 API 由宿主前端处理（展开/收起弹出层）
      if (api === 'dock.expand' || api === 'dock.collapse') {
        return handleExpand(pid, api === 'dock.expand' ? (args && args.height) || 160 : 0);
      }
      if (api === 'stats.subscribe') statsSubs.add(pid);      // 记录谁订了采样
      if (api === 'stats.unsubscribe') statsSubs.delete(pid);
      return T.core.invoke('plugin_call', { id: pid, api, args: args || {} });
    },
    on(pid, event, cb) { listeners.push({ pid, event, cb }); },
    jslog(m) { jslog(m); },
    // 插件渲染完成：量一次条带内容宽度并冻结—— dock 上每个插件的长度在
    // 加载时自适应、之后永久稳定（动态文本如网速/分支名不再引起 dock 变长缩短）。
    // 宽度可经设置中心的手动宽度调整后重载重新自适应。
    _ready(pid) { freezeWidth(frames[pid]); layout(); },
  };

  /// 冻结插件条带宽度（仅 ready 时调用，之后 layout() 一律使用冻结值，
  /// 不再随内容抖动）：
  /// - 用户手动调过宽度：严格用设定值，内容再宽也只裁剪不顶开（"缩不小"的修复）
  /// - 未调过：取 manifest 基准与内容实际宽度的较大者（加载时自适应）
  function freezeWidth(f) {
    if (!f) return;
    const card = f._card || f;
    const base = parseInt(card.dataset.baseWidth) || 160;
    let w = base;
    if (!card.dataset.customW) {
      try {
        const doc = f.contentDocument;
        const strip = doc && doc.getElementById('strip');
        if (strip) {
          const cw = stripContentWidth(doc, strip);
          if (cw > w) w = cw;
        }
      } catch (e) {}
    }
    if (f._maxW && w > f._maxW) w = f._maxW;
    f._frozenW = w;
  }

  async function handleExpand(pid, height) {
    if (expandedPid && expandedPid !== pid) {
      const prev = frames[expandedPid];
      if (prev) setPopupMode(prev, 0); // 还原为条带内嵌
      const w0 = prev && prev.contentWindow;
      if (w0) { try { w0.__tbp_emit('dock.collapsed', {}); } catch (e) {} }
    }
    expandedPid = height > 0 ? pid : null;
    if (height > 0) { rowPan = 0; applyPan(); } // 展开时归零平移：弹层宽度按未平移坐标计算
    const f = frames[pid];
    if (f) {
      if (height > 0) {
        // 展开顺序：先切 absolute 弹层再上报窗口尺寸。absolute 后面板超出当前
        // 小窗口的部分被窗口上缘自然裁切（不可见），条带位置即刻稳定；随后按
        // 真实内容高度贴合 iframe 与窗口，一次到位。
        setPopupMode(f, height);
        watchExpandedContent(f); // 内容动态长高（抓包进流量等）时持续跟进
        const fitH = Math.min(STRIP_H + MAX_POPUP_EXTRA,
          Math.max(STRIP_H + height, measureContent(f)));
        f.style.height = fitH + 'px';
        jslog('expand pid=' + pid + ' fitH=' + fitH);
        await layout(fitH - STRIP_H + 6);
      } else {
        // 收起：先还原条带内嵌再上报，窗口收缩期间条带高度由稳定基准兜底（layout 内处理）
        unwatchExpandedContent();
        setPopupMode(f, 0);
        await layout();
      }
    } else {
      unwatchExpandedContent();
    }
    await layout();
    // 内容异步渲染后再次测量贴合（两次，覆盖快/慢渲染）
    if (height > 0 && f) {
      setTimeout(() => refitExpanded(f), 60);
      setTimeout(() => refitExpanded(f), 300);
    }
  }

  /// 展开态贴合：内容异步长高（表格/日志载入）后把 iframe 与窗口跟进到真实高度
  async function refitExpanded(f) {
    if (!f || frames[expandedPid] !== f) return;
    const fitH = Math.min(STRIP_H + MAX_POPUP_EXTRA,
      Math.max(STRIP_H + 40, measureContent(f)));
    if (Math.abs(parseInt(f.style.height) - fitH) > 2) {
      f.style.height = fitH + 'px';
    }
    await layout();
  }

  /// 条带内容实际宽度（含条带自身 padding，不受容器拉伸影响）。
  /// 不能用 strip.scrollWidth：宿主 body 是 flex 列布局，#strip 会被拉伸到
  /// iframe 视口宽度（视口 = 卡片宽度 = 上次测量值），scrollWidth ≥ 视口宽度，
  /// 于是窗口宽度 ↔ 测量值形成正反馈环路——窗口每次收窄 2px，条带跟着变窄、
  /// 上报更小宽度、窗口再收窄（日志中 w 逐步 -2 的"磨盘"抖动）。Range 测内容
  /// 包围盒与视口无关，彻底断环。
  function stripContentWidth(doc, strip) {
    try {
      if (doc.createRange) {
        const rg = doc.createRange();
        rg.selectNodeContents(strip);
        let cw = Math.ceil(rg.getBoundingClientRect().width);
        const cs = doc.defaultView.getComputedStyle(strip);
        cw += (parseFloat(cs.paddingLeft) || 0) + (parseFloat(cs.paddingRight) || 0);
        if (cw > 0) return cw;
      }
    } catch (e) {}
    return strip.scrollWidth;
  }

  /// 弹出层模式：iframe 脱离文档流，锚定卡片右下、向左展开——
  /// 条带布局与宽度完全不变，任务栏不会随弹层变宽
  function setPopupMode(f, height) {
    if (height > 0) {
      let popupW = 64;
      let pel = null;
      try {
        const doc = f.contentDocument;
        const strip = doc && doc.getElementById('strip');
        // 弹层容器：#panel（多数插件）或 #detail（监控插件）
        pel = doc && (doc.getElementById('panel') || doc.getElementById('detail'));
        const sw = strip ? (f._frozenW || stripContentWidth(doc, strip)) + 4 : 64;
        const pw = pel && pel.classList.contains('open') ? pel.offsetWidth + 14 : 0;
        popupW = Math.max(sw, pw, parseInt(f._card.style.width) || 0);
      } catch (e) {}
      f._popupW = popupW;
      f.style.position = 'absolute';
      f.style.right = '0';
      f.style.bottom = '0';
      f.style.zIndex = '5';
      f.style.width = popupW + 'px';
      f.style.height = (STRIP_H + height) + 'px';
      // 弹层 iframe 比卡片宽：插件文档里的 #strip 会被容器拉伸到整个 iframe
      // 视口宽度，条带底色实际"变长"并藏在邻居插件下方（长度不该变化）。
      // 注入 CSS 把条带锁定为卡片宽度、右对齐（iframe 右锚定到卡片，条带
      // 视觉与收起时逐像素一致）；收起时移除。对所有插件（含第三方）通用。
      try {
        const doc = f.contentDocument;
        const cardW = parseInt(f._card.style.width) || 0;
        if (doc && cardW > 0) {
          let st = doc.getElementById('tbp-popup-css');
          if (!st) {
            st = doc.createElement('style');
            st.id = 'tbp-popup-css';
            (doc.head || doc.body).appendChild(st);
          }
          st.textContent = '#strip{width:' + cardW + 'px!important;margin-left:auto!important;margin-right:0!important;}'
            + 'html{scrollbar-width:none!important;}'
            + '::-webkit-scrollbar{display:none!important;width:0!important;height:0!important;}';
        }
      } catch (e) {}
      // 弹层 iframe 比卡片宽（向左展开），absolute 后会压住邻近卡片的条带层
      // 并拦截鼠标（"边上按钮被挡住"）。把同排其它卡片提到弹层之上：弹层在
      // 条带行是透明的、视觉无变化，面板区域在条带上方与邻居无重叠——邻居
      // 按钮恢复可点，本插件条带仍由弹层 iframe 自身响应。
      const row = f._card.parentNode;
      if (row) {
        for (const sib of row.children) {
          if (sib !== f._card && sib._zi == null) {
            sib._zi = sib.style.zIndex || '';
            sib.style.zIndex = '6';
          }
        }
      }
    } else {
      f._popupW = 0;
      f.style.position = '';
      f.style.right = '';
      f.style.bottom = '';
      f.style.zIndex = '';
      f.style.width = '100%';
      f.style.height = '100%';
      try {
        const doc = f.contentDocument;
        const st = doc && doc.getElementById('tbp-popup-css');
        if (st) st.remove();
      } catch (e) {}
      const row = f._card.parentNode;
      if (row) {
        for (const sib of row.children) {
          if (sib._zi != null) { sib.style.zIndex = sib._zi; delete sib._zi; }
        }
      }
    }
  }

    // 当前窗口需要的额外高度：右键菜单 > 插件展开（按内容实际高度贴合，避免黑框过大）
  let menuOpen = false;

  /// 可靠测量插件文档内容高度：插件 body 是 height:100vh 的 flex 底对齐容器，
  /// 任何视口高度下 scrollHeight 都等于视口（被"顶满"，不可用于测量）。
  /// 改为遍历 body 直接子元素取包围盒（含 margin）：底对齐布局下
  /// 内容不足时子元素贴底、内容超出时首个子元素 top 为负，两种情形都正确。
  function measureContent(f) {
    try {
      const d = f.contentDocument;
      const b = d && d.body;
      if (!b) return 0;
      let top = Infinity, bottom = 0;
      for (const kid of b.children) {
        const r = kid.getBoundingClientRect();
        if (r.width === 0 && r.height === 0) continue;
        const s = d.defaultView.getComputedStyle(kid);
        top = Math.min(top, r.top - (parseFloat(s.marginTop) || 0));
        bottom = Math.max(bottom, r.bottom + (parseFloat(s.marginBottom) || 0));
      }
      if (!isFinite(top)) return 0;
      return Math.max(STRIP_H, Math.ceil(bottom - top));
    } catch (e) { return 0; }
  }

  function currentExtra() {
    if (menuOpen) return 116;
    if (expandedPid && frames[expandedPid]) {
      const f = frames[expandedPid];
      const sh = measureContent(f);
      if (sh > STRIP_H) {
        // +6：给弹层顶部留足冗余，面板边框/圆角不被窗口上缘裁切（"上边界被吃掉"）
        return Math.min(600, sh - STRIP_H + 6);
      }
      return Math.max(0, parseInt(f.style.height) - STRIP_H);
    }
    return 0;
  }

  // ==================== 右键菜单 ====================

  function ensureCtxMenu() {
    let m = document.getElementById('ctx-menu');
    if (m) return m;
    m = document.createElement('div');
    m.id = 'ctx-menu';
    m.innerHTML = `
      <div class="ctx-item" data-act="settings">打开设置</div>
      <div class="ctx-sep"></div>
      <div class="ctx-item ctx-danger" data-act="quit">退出 TaskBar Plus</div>`;
    document.body.appendChild(m);
    m.addEventListener('click', async (e) => {
      const act = e.target.dataset && e.target.dataset.act;
      closeCtxMenu();
      if (act === 'settings') await T.core.invoke('open_settings');
      if (act === 'quit') await T.core.invoke('quit_app');
    });
    return m;
  }

  function openCtxMenu(x) {
    const m = ensureCtxMenu();
    m.style.display = 'block';
    m.style.left = Math.max(4, Math.min(x, window.innerWidth - 170)) + 'px';
    menuOpen = true;
    layout();
  }

  function closeCtxMenu() {
    const m = document.getElementById('ctx-menu');
    if (m) m.style.display = 'none';
    if (menuOpen) {
      menuOpen = false;
      layout();
    }
  }

  // Rust 事件 → 插件广播
  let statsReceived = { n: 0 };
  async function bindEvents() {
    await T.event.listen('stats', (e) => { statsReceived.n++; emit('stats', e.payload); });
    await T.event.listen('proxy', (e) => emit('proxy', e.payload));
    await T.event.listen('plugins-changed', async () => { await loadPlugins(); });
    await T.event.listen('settings-changed', async (e) => {
      // 轻量重拉插件元数据（含按插件 custom_css），只重注样式不重建 iframe
      try { pluginList = await T.core.invoke('plugins_list'); } catch (err) {}
      applySettings(e.payload || {});
    });
  }
  function emit(event, payload) {
    for (const l of listeners) {
      if (l.event !== event) continue;
      const f = frames[l.pid];
      if (!f || !f.contentWindow) continue;
      try { f.contentWindow.__tbp_emit(event, payload); } catch (err) {}
    }
  }

  // ==================== 插件加载 ====================

  let settings = { general: { spacing: 8, opacity: 100, max_width: 700 } };
  let pluginList = [];
  let settledStripH = 0; // resize 事件沉淀的条带稳定高度（过渡帧 innerHeight 滞后不可信，见 layout）
  let rowPan = 0;  // 插件行水平平移量：0 = 右对齐（左侧插件藏起），panMax = 左侧完全可见
  let panMax = 0;

  function applyPan() {
    const row = document.getElementById('plugin-row');
    if (row) row.style.transform = rowPan > 0 ? 'translateX(' + rowPan + 'px)' : '';
  }

  // ---- 窗口高度变化时的"伪装帧"：Windows/DWM 在 SetWindowPos 改变窗口高度的瞬间，
  // 会把 WebView 上一帧画面拉伸填充新窗口（展开时条带被拉到上方整体闪现、收起时
  // 压成一条残影）。在真正上报尺寸前，先让合成器提交一帧"伪装画面"：
  //   展开——把最终布局整体纵向缩进当前小窗口（scale = 当前高/目标高）。
  //         随后 DWM 按 目标高/当前高 拉伸，两者互为逆变换，中间帧恰为正确最终画面。
  //   收起——直接隐藏内容，中间帧拉伸空白（残影比"放大条带"更难察觉）。
  // 该帧提交后才会上报尺寸；窗口落地（resize 事件 innerHeight 匹配目标）或超时后恢复。
  let _pretend = null; // {dock, targetH, timer}
  function restorePretend() {
    if (!_pretend) return;
    clearTimeout(_pretend.timer);
    const d = _pretend.dock;
    d.style.height = ''; d.style.transform = ''; d.style.transformOrigin = ''; d.style.visibility = '';
    _pretend = null;
  }
  function beginPretend(targetH) {
    const dock = document.getElementById('dock');
    if (!dock) return Promise.resolve();
    const curH = window.innerHeight;
    if (targetH > curH) {
      dock.style.height = targetH + 'px';
      dock.style.transformOrigin = '0 0';
      dock.style.transform = 'scale(1,' + (curH / targetH) + ')';
    } else {
      dock.style.visibility = 'hidden';
    }
    _pretend = { dock, targetH, timer: setTimeout(restorePretend, 400) };
    // 双 rAF + 33ms：确保合成器确实把伪装帧提交给 DWM，再让调用方上报尺寸
    return new Promise(res => requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(res, 33))));
  }

  async function loadPlugins() {
    pluginList = await T.core.invoke('plugins_list');
    applySettings();
    const row = document.getElementById('plugin-row');
    // 先退订再摘 iframe：所有插件文档即将销毁，它们持有的 stats 采样订阅
    // 必须在此归零（Rust 侧计数器无法感知 iframe 死亡），禁用/重载后采样立即停。
    // await 保证 Rust 处理完退订后才重建，避免与新版插件的订阅竞态
    await Promise.all([...statsSubs].map(pid =>
      T.core.invoke('plugin_call', { id: pid, api: 'stats.unsubscribe', args: {} }).catch(() => {})));
    if (statsSubs.size) jslog('teardown unsubscribe: ' + [...statsSubs].join(','));
    statsSubs.clear();
    row.innerHTML = '';
    unwatchExpandedContent();
    for (const k in frames) delete frames[k];
    listeners.length = 0;
    expandedPid = null;

    const enabled = pluginList.filter(p => p.enabled);
    for (const p of enabled) {
      const src = await T.core.invoke('plugin_source', { id: p.id });
      if (!src) continue;
      // 每个插件包一层卡片壳（独立圆角/底色/描边，见 style.css .plg-card）
      const card = document.createElement('div');
      card.className = 'plg-card';
      card.dataset.baseWidth = p.resolved_width;
      if (p.width_customized) card.dataset.customW = '1';
      card.style.width = p.resolved_width + 'px';
      card.style.height = STRIP_H + 'px';
      const f = document.createElement('iframe');
      f.className = 'plugin-frame';
      f._card = card;
      // 弹层可能比条带宽（图标类插件）：绝对上限，manifest width.max 只约束条带宽度
      f._maxW = 560;
      f.setAttribute('sandbox', 'allow-scripts allow-same-origin allow-downloads');
      f.srcdoc = buildSrcdoc(p.id, src);
      // 注入用户 CSS：iframe 每次加载（含内部跳转）后重注，防止插件脚本清空 head
      f.addEventListener('load', () => {
        try { injectStyle(f.contentDocument, 'tbp-user-css', userCssFor(p.id)); } catch (e) {}
      });
      card.appendChild(f);
      document.getElementById('plugin-row').appendChild(card);
      frames[p.id] = f;
    }
    layout();
  }

  function buildSrcdoc(pid, html) {
    const runtime = `<script>
      window.__tbp_listeners = {};
      window.__tbp_emit = function (event, payload) {
        (window.__tbp_listeners[event] || []).forEach(function (cb) {
          try { cb(payload); } catch (e) {}
        });
      };
      window.onerror = function (m, s, l) {
        try { window.parent.TBP_HOST && window.parent.TBP_HOST.jslog && window.parent.TBP_HOST.jslog('plugin ' + ${JSON.stringify(pid)} + ' error: ' + m + ' @' + l); } catch (e) {}
      };
      window.addEventListener('unhandledrejection', function (e) {
        try { window.parent.TBP_HOST && window.parent.TBP_HOST.jslog && window.parent.TBP_HOST.jslog('plugin ' + ${JSON.stringify(pid)} + ' rejection: ' + (e.reason && e.reason.message || e.reason)); } catch (e2) {}
      });
      window.tbp = {
        id: ${JSON.stringify(pid)},
        ready: function () { window.parent.TBP_HOST._ready(${JSON.stringify(pid)}); },
        call: function (api, args) { return window.parent.TBP_HOST.call(${JSON.stringify(pid)}, api, args); },
        on: function (event, cb) {
          // 本地注册（供 __tbp_emit 派发）+ 父窗口路由记录（供宿主转发事件）
          (window.__tbp_listeners[event] = window.__tbp_listeners[event] || []).push(cb);
          window.parent.TBP_HOST.on(${JSON.stringify(pid)}, event, cb);
        },
        config: function () { return this.call('config.get'); },
        saveConfig: function (c) { return this.call('config.set', { config: c }); }
      };
    <\/script>`;
    // 运行时必须在插件自身脚本之前注入：插到 <body> 后，兜底放 </head> 前或追加
    if (/<body[^>]*>/i.test(html)) return html.replace(/<body[^>]*>/i, (m) => m + runtime);
    if (html.includes('</head>')) return html.replace('</head>', runtime + '</head>');
    return runtime + html;
  }

  // ==================== 布局与上报 ====================

  async function layout(forcedExtra) {
    const row = document.getElementById('plugin-row');
    const g = settings.general || {};
    document.documentElement.style.setProperty('--spacing', (g.spacing != null ? g.spacing : 8) + 'px');
    // 条带高度：稳态下 innerHeight - extra 恒等于收起态高度（展开/收起/常态都成立）。
    // 但过渡帧中窗口尺寸尚未跟上刚上报的 extra（展开瞬间被压矮、收起瞬间被拉高），
    // 此时 innerHeight 不可信——与 resize 事件沉淀的稳定值偏差 >8px 即判定为过渡帧，
    // 回退到稳定值，条带高度全程不变（防点击后条带背景闪高/压矮的抖动）
    const extraNow = forcedExtra != null ? forcedExtra : currentExtra();
    let stripH = window.innerHeight - extraNow;
    if (settledStripH >= 32 && Math.abs(stripH - settledStripH) > 4) stripH = settledStripH;
    stripH = Math.max(32, stripH);
    document.documentElement.style.setProperty('--tb-h', stripH + 'px');
    layout._n = (layout._n || 0) + 1;
    // 各插件宽度一律使用 ready 时冻结的值（freezeWidth），不再随内容实时
    // 变化——dock 条长度稳定，不会莫名变长缩短；未 ready 前先用 manifest 基准
    let total = 0;
    const ids = Object.keys(frames);
    const sp = g.spacing != null ? g.spacing : 8;
    for (const pid of ids) {
      const f = frames[pid];
      const card = f._card || f;
      const base = parseInt(card.dataset.baseWidth) || 160;
      let w = f._frozenW || base;
      if (f._maxW && w > f._maxW) w = f._maxW;
      f._w = w;
      card.style.width = w + 'px';
      total += w;
    }
    total += Math.max(0, ids.length - 1) * sp;

    // 卡片"未平移"坐标系中的左偏移（弹层窗口宽度计算用；展开时已归零平移，视觉一致）
    let acc = 0;
    for (let i = 0; i < ids.length; i++) {
      const f = frames[ids[i]];
      f._cardX = acc;
      acc += f._w + (i < ids.length - 1 ? sp : 0);
    }

    // 长度限制：超过最大长度时窗口不再变宽，按住拖拽平移查看被隐藏的插件（无滚动条）。
    // 窗口边缘天然裁掉溢出内容；右侧插件滑入尾部下方（尾部 z-index 更高）
    const tail = document.getElementById('dock-tail');
    const tailW = tail.offsetWidth + 8;
    const maxW = g.max_width || 700;
    let winW = Math.min(total + tailW + 8, maxW);
    const rowAvail = Math.max(0, winW - tailW - 8);
    panMax = Math.max(0, total - rowAvail);
    if (rowPan > panMax) rowPan = panMax;
    applyPan();
    document.getElementById('dock').classList.toggle('pannable', panMax > 0);

    const extra = extraNow;
    // 窗口宽度：内容宽度，或容纳展开弹层所需的最小宽度。
    // 弹层右锚定卡片、向左展开：最左侧插件的弹层左缘可能超出窗口左缘被裁
    // （剪切板"弹窗不全"的根因），必须按弹层实际左缘扩展窗口（向左透明延伸，
    // 行内容右对齐，可见布局不变）。
    const ef = expandedPid && frames[expandedPid];
    if (ef && ef._popupW && ef._cardX != null) {
      const need = tailW + acc - ef._cardX - ef._w + ef._popupW + 12;
      if (need > winW) winW = need;
    }
    // 插件重载瞬间 frames 为空：保持上次上报的宽度，避免 dock 缩成 60px 小点（"dock 消失"）
    if (!ids.length && layout._lastW) winW = layout._lastW;
    layout._lastW = Math.round(winW);
    const repId = Math.round(winW) + '/' + extra + '/' + rowPan;
    if (repId !== layout._lastRep) {
      layout._lastRep = repId;
      jslog('layout report w=' + Math.round(winW) + ' extra=' + extra + ' tbh=' + stripH +
        (forcedExtra != null ? ' forced' : '') + ' pan=' + rowPan + '/' + panMax +
        ' cards=' + ids.map(p => frames[p]._w).join(','));
    }
    // 伪装帧：窗口高度即将变化时，先提交一帧伪装画面，避免 DWM 拉伸旧帧导致
    // "dock 条整体在上方闪现"（详见 beginPretend 注释）。高度未变不触发。
    const targetH = stripH + extra;
    if (!_pretend && Math.abs(window.innerHeight - targetH) > 6) {
      await beginPretend(targetH);
    } else if (_pretend && Math.abs(window.innerHeight - _pretend.targetH) <= 6) {
      restorePretend(); // 高度已落地：撤掉伪装，恢复正常布局
    }
    await T.core.invoke('dock_report_size', {
      width: Math.round(winW),
      extraHeight: extra,
      hitRects: hitRectsFor(winW, stripH + extra, stripH),
    });
  }

  /// 可点击区域（窗口客户区坐标，CSS px）→ Rust 侧 WM_NCHITTEST 穿透白名单：
  /// 条带行 + 展开弹层。其余区域（弹层外的空白带、向左透明延伸区）穿透到后方
  /// 程序（"隐藏层挡住后面窗口点不到"的根治）。
  /// 不用 getBoundingClientRect：伪装帧期间 dock 整体被 scale 压缩，测量值
  /// 是变换后的视觉矩形（错误）；改用布局数学（右下锚定 + 已知宽高），与变换无关。
  /// 返回 null = 整个窗口可点击（未展开/未开菜单，窗口本就贴合条带，无需穿透）。
  function hitRectsFor(winW, winH, stripH) {
    try {
      if (menuOpen) return [[0, 0, Math.round(winW), Math.round(winH)]];
      const ef = expandedPid && frames[expandedPid];
      if (!ef) return null;
      const pw = Math.min(Math.round(ef._popupW || winW), Math.round(winW));
      const ph = Math.min(parseInt(ef.style.height) || Math.round(winH), Math.round(winH));
      return [
        [0, Math.round(winH - stripH), Math.round(winW), Math.round(stripH)],
        [Math.round(winW - pw), Math.round(winH - ph), pw, ph],
      ];
    } catch (e) { return null; }
  }

  function hexToRgba(hex, alpha) {
    const m = /^#?([0-9a-f]{6})$/i.exec(String(hex).trim());
    if (!m) return 'rgba(31,32,37,' + alpha + ')';
    const n = parseInt(m[1], 16);
    return 'rgba(' + (n >> 16) + ',' + ((n >> 8) & 255) + ',' + (n & 255) + ',' + alpha + ')';
  }

  function applySettings(s) {
    if (s) settings = s;
    const g = settings.general || {};
    const op = (g.opacity != null ? g.opacity : 100) / 100;
    // 背景：用户自定义颜色（可全透明）> 预设皮肤/默认深色。
    // 预设皮肤的 CSS 用 !important 声明了 --dock-bg / --dock-bg-img，
    // 普通内联样式赢不了它（"设置了背景颜色/图片无效、透明底穿透不出来"的根因），
    // 这里以内联 !important 写回（内联 important > 样式表 important），
    // 用户显式设置即夺回优先级；未设置时移除内联值，预设皮肤恢复生效。
    const rootStyle = document.documentElement.style;
    if (g.bg_color) {
      // 主「背景不透明度」滑杆 (g.opacity) 与颜色行内的透明度相乘：
      // 两个滑杆任何时候都可见生效（此前自定义颜色不吃主滑杆，"完全看不出效果"）
      const alpha = ((g.bg_opacity != null ? g.bg_opacity : 92) / 100) * op;
      rootStyle.setProperty('--dock-bg',
        hexToRgba(g.bg_color, alpha.toFixed(2)), 'important');
    } else if ((settings.general || {}).theme && THEMES[(settings.general || {}).theme]) {
      rootStyle.removeProperty('--dock-bg'); // 有预设皮肤：交给皮肤的 !important
    } else {
      rootStyle.setProperty('--dock-bg',
        'rgba(31,32,37,' + (0.92 * op).toFixed(2) + ')');
    }
    // 背景图片（data URL；空 = 无）
    if (g.bg_image) {
      rootStyle.setProperty('--dock-bg-img', 'url("' + g.bg_image + '")', 'important');
    } else {
      rootStyle.removeProperty('--dock-bg-img');
    }
    applyUserCss();
    layout();
  }

  // ==================== 用户自定义 CSS 注入 ====================
  // 全局 CSS：宿主文档 + 所有插件 iframe；按插件 CSS：仅该插件 iframe。
  // 插件是 srcdoc + allow-same-origin，宿主可直接写 iframe 文档。
  function injectStyle(doc, id, css) {
    if (!doc) return;
    let el = doc.getElementById(id);
    if (!el) {
      el = doc.createElement('style');
      el.id = id;
      (doc.head || doc.documentElement).appendChild(el);
    }
    if (el.textContent !== css) el.textContent = css;
  }

  function userCssFor(pid) {
    const themeCss = THEMES[((settings.general || {}).theme)] || '';
    const g = ((settings.general || {}).custom_css || '');
    const entry = pluginList.find(x => x.id === pid);
    const p = (entry && entry.custom_css) || '';
    return themeCss + '\n' + g + '\n' + p;
  }

  function applyUserCss() {
    const themeCss = THEMES[((settings.general || {}).theme)] || '';
    const g = ((settings.general || {}).custom_css || '');
    injectStyle(document, 'tbp-global-css', themeCss + '\n' + g); // 宿主只注 预设+全局
    for (const id in frames) {
      const f = frames[id];
      try { injectStyle(f.contentDocument, 'tbp-user-css', userCssFor(id)); } catch (e) {}
    }
  }

  // ==================== 预设皮肤 ====================
  // 一键换肤：预设 CSS 与全局/按插件 CSS 走同一条注入通道，用户 CSS 在后可覆盖预设。
  // 宿主 dock 条背景 = #strip-bar::before 的 --dock-bg / --dock-bg-img；
  // 各插件条内部约定 #strip、弹层约定 #detail / #panel（内置插件均遵循）。
  const THEMES = {
    // 毛玻璃：半透明 +  backdrop 模糊，透出桌面壁纸
    glass:
      ':root{--dock-bg:rgba(42,44,54,.42)!important;--dock-bg-img:none!important}\n' +
      '#strip-bar::before{backdrop-filter:blur(16px) saturate(1.35);-webkit-backdrop-filter:blur(16px) saturate(1.35);border:1px solid rgba(255,255,255,.16);box-shadow:0 4px 20px rgba(0,0,0,.28)}\n' +
      '#strip{background:rgba(255,255,255,.08)!important;border-color:rgba(255,255,255,.18)!important;backdrop-filter:blur(10px)}\n' +
      '#detail,#panel{background:rgba(28,30,40,.78)!important;border-color:rgba(255,255,255,.16)!important;backdrop-filter:blur(18px)}\n' +
      '#ctx-menu{background:rgba(34,36,44,.85)!important;border-color:rgba(255,255,255,.14)!important;backdrop-filter:blur(14px)}',
    // 浅色：亮白底深灰字
    light:
      ':root{--dock-bg:rgba(246,247,251,.94)!important;--dock-bg-img:none!important}\n' +
      '#strip-bar::before{border:1px solid rgba(20,22,30,.1);box-shadow:0 2px 12px rgba(20,22,40,.14)}\n' +
      '#strip{background:#ffffff!important;border-color:#d9dce4!important}\n' +
      '#strip .lb,#strip .val,#strip .st,#strip .cell,#strip span,#strip div{color:#3a3c46!important}\n' +
      '#detail,#panel{background:#ffffff!important;border-color:#e3e6ee!important;color:#2b2d38!important}\n' +
      '#detail .hd,#panel .hd,#detail .grid,#panel .grid,#detail span,#panel span,#detail div,#panel div{color:#4a4c58!important}\n' +
      '#ctx-menu{background:#ffffff!important;border:1px solid #e3e6ee!important;color:#2b2d38}',
    // 高对比：纯黑底 + 白框 + 亮黄点缀
    contrast:
      ':root{--dock-bg:#000000!important;--dock-bg-img:none!important}\n' +
      '#strip-bar::before{border:2px solid #ffffff}\n' +
      '#strip{background:#000!important;border:2px solid #fff!important}\n' +
      '#strip *{color:#fff!important}\n' +
      '#detail,#panel{background:#000!important;border:2px solid #ffd800!important;color:#fff!important}\n' +
      '#detail *,#panel *{color:#fff!important}\n' +
      '#ctx-menu{background:#000!important;border:2px solid #fff!important;color:#fff!important}',
    // 棱镜：彩虹光谱流动
    prism:
      ':root{--dock-bg:linear-gradient(115deg,#ff5f6d,#ffc371,#47e891,#4c8dff,#a06bff,#ff5f6d)!important;--dock-bg-img:none!important}\n' +
      '#strip-bar::before{background-size:300% 100%!important;animation:tbp-prism 8s linear infinite;opacity:.92}\n' +
      '@keyframes tbp-prism{to{background-position:300% 0}}\n' +
      '#strip{background:rgba(20,20,26,.42)!important;border-color:rgba(255,255,255,.28)!important}\n' +
      '#detail,#panel{background:rgba(24,22,32,.95)!important;border-color:rgba(255,255,255,.2)!important}',
    // 星云：深空紫蓝 + 光晕缓慢漂移
    nebula:
      ':root{--dock-bg:#0b0618!important;--dock-bg-img:radial-gradient(ellipse 60% 140% at 12% 50%,rgba(138,92,255,.6),transparent 62%),radial-gradient(ellipse 50% 120% at 88% 25%,rgba(76,141,255,.5),transparent 60%),radial-gradient(ellipse 45% 100% at 55% 95%,rgba(255,95,158,.38),transparent 62%)!important}\n' +
      '#strip-bar::before{background-size:220% 220%!important;animation:tbp-nebula 18s ease-in-out infinite alternate}\n' +
      '@keyframes tbp-nebula{from{background-position:0 0}to{background-position:100% 100%}}\n' +
      '#strip{background:rgba(22,12,44,.62)!important;border-color:rgba(170,130,255,.4)!important}\n' +
      '#detail,#panel{background:rgba(15,9,32,.95)!important;border-color:rgba(170,130,255,.32)!important}',
    // 渐变：靛蓝→紫罗兰→品红柔和渐变
    gradient:
      ':root{--dock-bg:linear-gradient(90deg,#232a63,#3b5bdb,#7048e8,#ae3ec9)!important;--dock-bg-img:none!important}\n' +
      '#strip-bar::before{background-size:250% 100%!important;animation:tbp-grad 12s ease infinite}\n' +
      '@keyframes tbp-grad{0%,100%{background-position:0 0}50%{background-position:100% 0}}\n' +
      '#strip{background:rgba(255,255,255,.13)!important;border-color:rgba(255,255,255,.28)!important}\n' +
      '#detail,#panel{background:rgba(30,28,56,.96)!important;border-color:rgba(255,255,255,.2)!important}',
    // 流体：多团彩色流体相互渗透（2D 流体效果的 CSS 近似，色团持续变形流动）
    fluid:
      ':root{--dock-bg:#0e0e16!important;--dock-bg-img:radial-gradient(ellipse 45% 170% at 18% 55%,rgba(0,209,255,.75),transparent 56%),radial-gradient(ellipse 40% 150% at 62% 15%,rgba(255,0,170,.6),transparent 56%),radial-gradient(ellipse 45% 160% at 88% 85%,rgba(110,255,150,.55),transparent 56%),radial-gradient(ellipse 35% 130% at 38% 95%,rgba(255,196,0,.5),transparent 56%)!important}\n' +
      '#strip-bar::before{filter:blur(7px) saturate(1.5);background-size:200% 200%!important;animation:tbp-fluid 11s ease-in-out infinite alternate}\n' +
      '@keyframes tbp-fluid{from{background-position:0 0,100% 0,0 100%,100% 100%}to{background-position:70% 60%,25% 85%,85% 15%,15% 45%}}\n' +
      '#strip{background:rgba(14,14,22,.45)!important;border-color:rgba(255,255,255,.22)!important}\n' +
      '#detail,#panel{background:rgba(16,16,26,.94)!important;border-color:rgba(255,255,255,.18)!important}',
  };

  // ==================== 启动 ====================

  async function initDock() {
    settings = await T.core.invoke('settings_get');
    document.getElementById('btn-settings').onclick = () => {
      jslog('gear clicked');
      T.core.invoke('open_settings').catch((e) => jslog('open_settings err: ' + e));
    };
    // 右键菜单（dock 背景/尾部按钮区域；iframe 内部右键由插件文档自行处理）
    document.getElementById('dock').addEventListener('contextmenu', (e) => {
      e.preventDefault();
      if (e.target.closest('.plugin-frame')) return;
      openCtxMenu(e.clientX);
    });
    document.addEventListener('click', (e) => {
      if (menuOpen && !e.target.closest('#ctx-menu')) closeCtxMenu();
    });
    window.addEventListener('blur', closeCtxMenu);
    window.addEventListener('resize', () => {
      // 窗口几何稳定后才沉淀条带高度：稳态 innerHeight - extra 恒等于收起态高度；
      // 展开/收起的过渡帧不走这里（resize 由 SetWindowPos 落地后触发，值已稳定）
      const h = window.innerHeight - currentExtra();
      if (h >= 32) settledStripH = h;
      jslog('resize innerH=' + window.innerHeight + ' extra=' + currentExtra() + ' settled=' + settledStripH);
      layout();
    });
    // ---- 超长按住拖拽平移插件行（无滚动条）----
    // 监听绑在 #dock 上：卡片内部事件被 iframe 接收，间距/尾部天然到不了这里；
    // 左侧把手 #pan-grip 是 #dock 的直接子元素（若在 strip-bar 内会被平移带跑），
    // 事件冒泡到 #dock 才能触发拖拽。iframe 区域自己吞事件，不会误触发。
    // 拖动过程中把全部插件 iframe 临时 pointer-events:none，移动事件才能持续回到宿主。
    const bar = document.getElementById('dock');
    let panDrag = null;
    bar.addEventListener('pointerdown', (e) => {
      if (!panMax || e.button !== 0 || panDrag) return;
      if (e.target.closest('button')) return; // 尾部按钮保持点击行为
      panDrag = { pid: e.pointerId, x0: e.clientX, pan0: rowPan, moved: false };
      try { bar.setPointerCapture(e.pointerId); } catch (err) {}
      e.preventDefault();
      jslog('pan down @' + e.clientX + ' panMax=' + panMax + ' target=' + (e.target.id || e.target.className));
    });
    bar.addEventListener('pointermove', (e) => {
      if (!panDrag || e.pointerId !== panDrag.pid) return;
      const dx = e.clientX - panDrag.x0;
      if (!panDrag.moved && Math.abs(dx) > 3) {
        panDrag.moved = true;
        document.body.classList.add('panning');
        document.getElementById('dock').classList.add('panning');
      }
      if (panDrag.moved) {
        rowPan = Math.min(panMax, Math.max(0, panDrag.pan0 + dx));
        applyPan();
        if (!panDrag.logged) { panDrag.logged = true; jslog('pan move -> rowPan=' + rowPan + '/' + panMax); }
      }
    });
    const endPanDrag = (e) => {
      if (!panDrag || e.pointerId !== panDrag.pid) return;
      jslog('pan end moved=' + !!panDrag.moved + ' rowPan=' + rowPan);
      panDrag = null;
      document.body.classList.remove('panning');
      document.getElementById('dock').classList.remove('panning');
      layout(); // 归位后回写最终窗口几何
    };
    bar.addEventListener('pointerup', endPanDrag);
    bar.addEventListener('pointercancel', endPanDrag);
    await bindEvents();
    await loadPlugins();
    await T.core.invoke('dock_report_size', { width: 480, extraHeight: 0 });
    // 启动自检：6s 后报告 stats 事件是否到达（写日志文件，便于 release 排查）
    setTimeout(() => jslog('selfcheck: stats=' + statsReceived.n +
      ', listeners=' + listeners.length + ', frames=' + Object.keys(frames).length +
      ', innerH=' + window.innerHeight +
      ', tbh=' + getComputedStyle(document.documentElement).getPropertyValue('--tb-h').trim() +
      ', stripBarH=' + document.getElementById('strip-bar').offsetHeight +
      ', rowH=' + document.getElementById('plugin-row').offsetHeight +
      ', tailH=' + document.getElementById('dock-tail').offsetHeight +
      ', cards=' + [...document.querySelectorAll('.plg-card')].map(c => c.offsetHeight).join('/') +
      ', frames=' + [...document.querySelectorAll('.plg-card iframe')].map(c => c.offsetHeight).join('/') +
      ', cardsW=' + [...document.querySelectorAll('.plg-card')].map(c => {
        const f = c.querySelector('iframe');
        let has = '?';
        try { has = f && f.contentDocument && f.contentDocument.body ? f.contentDocument.body.textContent.length : 'no-doc'; } catch (e) { has = 'err'; }
        return c.offsetWidth + '/' + has;
      }).join('|')), 6000);
  }
})();
