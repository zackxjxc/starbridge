'use strict';

(() => {
  const i18n = window.StarBridgeI18n;
  const t = i18n.t;
  const root = document.getElementById('app');
  const menu = document.getElementById('context-menu');
  const modal = document.getElementById('modal');
  const toastNode = document.getElementById('toast');
  const languageMenu = document.getElementById('language-menu');
  const desktop = window.__TAURI__?.core;
  const frameless = !!desktop && window.__STARBRIDGE_FRAMELESS__ === true;
  const closeLabel = window.__STARBRIDGE_TRAY__ === true ? '关闭到托盘' : '关闭';
  function syncDesktopLocale() {
    return desktop?.invoke('client_set_desktop_locale', { locale: i18n.locale() })
      .catch(error => console.warn('Could not update the desktop language', error));
  }
  const activeStates = new Set(['negotiating', 'connected']);
  const workingStates = new Set(['queued', 'connecting', 'verifying']);
  const ui = {
    snapshot: null, config: null, view: 'device', selected: null, query: '',
    authMode: 'login', auth: { username: '', password: '', confirm_password: '', remember: true },
    authError: '', loadError: '', syncError: '', busy: new Set(), drafts: new Map(),
    policy: 'direct_preferred', rebuild: false, allConfig: true, failureGroup: null,
    components: null, exportData: null, exportIds: [], exportFingerprint: '', exportBack: 'device',
    historyItems: [], historyTotal: 0, historyOffset: 0, historyLoaded: false, historyError: '', historyRequest: 0,
    historyDirection: 'all', historyFailed: false, historyAscending: false, historyExpanded: null,
    languageOpen: false, modalRender: null,
    epoch: 0, poll: null, request: 0, applied: 0, mutating: 0, mutation: 0, mutationApplied: 0, menuNode: null, menuReturn: null, modalKind: null,
  };
  const routes = {
    client_state: ['GET', '/api/v1/state'], client_config: ['GET', '/api/v1/config'],
    client_save_config: ['PUT', '/api/v1/config'], client_login: ['POST', '/api/v1/login'],
    client_register: ['POST', '/api/v1/register'], client_logout: ['POST', '/api/v1/logout'],
    client_update_device: ['PUT', '/api/v1/devices'], client_connect: ['POST', '/api/v1/connect'],
    client_delete_device: ['POST', '/api/v1/devices/delete'],
    client_rebind_device: ['POST', '/api/v1/devices/rebind'],
    client_connect_all: ['POST', '/api/v1/connect/all'], client_terminate: ['POST', '/api/v1/terminate'],
    client_export: ['POST', '/api/v1/export'], client_start_test: ['POST', '/api/v1/test/start'],
    client_connection_history: ['POST', '/api/v1/history'],
    client_cancel_test: ['POST', '/api/v1/test/cancel'], client_components: ['GET', '/api/v1/components'],
  };
  async function api(command, args = {}) {
    if (desktop) return desktop.invoke(command, args);
    const [method, url] = routes[command];
    const options = { method, headers: { 'Content-Type': 'application/json' }, cache: 'no-store' };
    if (method !== 'GET') options.body = JSON.stringify(args.form ?? args.config ?? {});
    const response = await fetch(url, options);
    const body = await response.text();
    let result;
    try { result = body ? JSON.parse(body) : null; } catch { throw new Error(t("客户端返回了无法读取的响应，请稍后重试。")); }
    if (!response.ok) throw new Error(result?.error || result?.message || t("请求未完成（{0}）", response.status));
    return result;
  }
  function message(value) {
    const raw = value instanceof Error ? value.message : typeof value === 'string' ? value : value?.message || t("操作未完成，请重试。");
    return i18n.source(raw).replace(/https?:\/\/[^\s)）]+/g, '[service address]');
  }
  const displayMessage = value => i18n.message(message(value)).replaceAll('[service address]', t('服务地址'));
  function h(tag, attrs = {}, ...children) {
    const element = document.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (value === null || value === undefined || value === false) continue;
      if (key === 'value') element.value = String(value);
      else if (key === 'checked' || key === 'disabled' || key === 'readOnly') element[key] = Boolean(value);
      else element.setAttribute(key === 'className' ? 'class' : key, String(value));
    }
    for (const child of children.flat(Infinity)) {
      if (child !== null && child !== undefined && child !== false) element.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return element;
  }
  const paths = {
    monitor: 'M3 4h18v12H3z M8 21h8 M12 16v5', search: 'M10 3a7 7 0 1 0 0 14 7 7 0 0 0 0-14 M15 15l6 6',
    arrow: 'm9 5 7 7-7 7', back: 'm15 5-7 7 7 7', grid: 'M3 3h7v7H3z M14 3h7v7h-7z M3 14h7v7H3z M14 14h7v7h-7z',
    test: 'M9 3h6 M10 3v6L4 19q-1 2 2 2h12q3 0 2-2L14 9V3 M8 14h8',
    more: 'M5 12h.01 M12 12h.01 M19 12h.01', check: 'm5 12 4 4L19 6', close: 'm6 6 12 12 M18 6 6 18',
    copy: 'M9 9h11v12H9z M15 9V3H3v12h6', refresh: 'M20 7A9 9 0 1 0 21 15 M20 2v5h-5', minimize: 'M5 12h14',
    history: 'M3 12a9 9 0 1 0 3-6 M3 3v6h6 M12 7v5l3 2',
    globe: 'M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0 M3 12h18 M12 3a17 17 0 0 1 0 18 17 17 0 0 1 0-18',
    down: 'm6 9 6 6 6-6',
  };
  function icon(name) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 24 24'); svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS(svg.namespaceURI, 'path');
    path.setAttribute('d', paths[name] || paths.monitor); path.setAttribute('stroke-linecap', 'round'); path.setAttribute('stroke-linejoin', 'round');
    if (name === 'more') path.setAttribute('stroke-width', '3');
    svg.append(path); return svg;
  }
  function button(label, action, attrs = {}, symbol) {
    return h('button', { type: 'button', class: 'button', 'data-action': action, ...attrs }, symbol ? icon(symbol) : null, label);
  }
  // Keyed reconciliation keeps search, edit drafts, selection and scroll stable across polls.
  function patch(current, next) {
    if (current.nodeType !== next.nodeType || current.nodeName !== next.nodeName) { current.replaceWith(next); return next; }
    if (current.nodeType === Node.TEXT_NODE) { if (current.data !== next.data) current.data = next.data; return current; }
    for (const attr of [...current.attributes]) {
      if (current instanceof HTMLDetailsElement && attr.name === 'open') continue;
      if (!next.hasAttribute(attr.name)) current.removeAttribute(attr.name);
    }
    for (const attr of [...next.attributes]) if (current.getAttribute(attr.name) !== attr.value) current.setAttribute(attr.name, attr.value);
    if (current instanceof HTMLInputElement || current instanceof HTMLTextAreaElement) {
      if (current.value !== next.value && (document.activeElement !== current || next.readOnly)) current.value = next.value;
      current.disabled = next.disabled; current.readOnly = next.readOnly;
      if (current instanceof HTMLInputElement) current.checked = next.checked;
    }
    if (current instanceof HTMLButtonElement) current.disabled = next.disabled;
    if (current instanceof HTMLProgressElement) { current.max = next.max; current.value = next.value; }
    if (!(current instanceof HTMLTextAreaElement)) patchChildren(current, [...next.childNodes]);
    return current;
  }
  function patchChildren(parent, fresh) {
    const old = [...parent.childNodes]; const used = new Set();
    fresh.forEach((next, index) => {
      const key = next.nodeType === 1 ? next.getAttribute('data-key') : null;
      let existing = key ? old.find(node => node.nodeType === 1 && node.getAttribute('data-key') === key) : old[index];
      if (existing && (used.has(existing) || (!key && existing.nodeType === 1 && existing.hasAttribute('data-key')))) existing = null;
      if (existing) { used.add(existing); existing = patch(existing, next); }
      else existing = next;
      const at = parent.childNodes[index]; if (at !== existing) parent.insertBefore(existing, at || null);
    });
    for (const node of old) if (!used.has(node) && node.parentNode === parent) node.remove();
  }
  const s = () => ui.snapshot;
  const localId = () => s()?.node_id;
  const localRegistered = () => !!s()?.logged_in && !s()?.device_rebind_required
    && !['registering', 'device_registration_failed'].includes(s()?.stage)
    && (s()?.devices || []).some(item => item.node_id === localId());
  const sessions = () => (s()?.sessions || []).filter(session => activeStates.has(session.status));
  const outgoing = () => sessions().filter(session => session.initiator_node_id === localId());
  const incoming = () => sessions().filter(session => session.target_node_id === localId());
  const test = () => s()?.self_test || { state: 'idle', steps: [] };
  const testBusy = () => ['running', 'cleaning'].includes(test().state);
  const ops = () => s()?.device_operations || [];
  const operation = id => ops().find(item => item.node_id === id);
  const isEnding = item => item?.online && operation(item.node_id)?.state === 'terminated'
    && item.occupancy && item.occupancy !== 'idle' && item.occupied_by === localId();
  const peers = () => (s()?.devices || []).filter(device => device.node_id !== localId());
  function device(id) {
    if (id === localId()) {
      return { ...(s()?.devices || []).find(item => item.node_id === id), node_id: id, display_name: s()?.display_name || t("这台电脑"),
        purpose: s()?.purpose || '', os: s()?.platform || (s()?.devices || []).find(item => item.node_id === id)?.os || '', online: true };
    }
    return peers().find(item => item.node_id === id);
  }
  const name = id => device(id)?.display_name || t("其他设备");
  const sessionFor = id => sessions().find(item => item.target_node_id === id && item.initiator_node_id === localId()) || sessions().find(item => item.initiator_node_id === id && item.target_node_id === localId());
  const ready = id => {
    const session = sessionFor(id);
    return session?.initiator_node_id === localId() && session.status === 'connected' && session.ssh_ready === true && !!session.ssh_command;
  };
  const readySessions = () => outgoing().filter(session => ready(session.target_node_id));
  const hasWork = () => sessions().length > 0 || ops().some(item => workingStates.has(item.state)) || s()?.batch?.running;
  const hasOutgoingWork = () => outgoing().length > 0 || ops().some(item => workingStates.has(item.state)) || s()?.batch?.running;
  const pathLabel = path => ({ direct: t("直连"), relay: t("中转"), unknown: t("正在协商") })[path] || t("正在协商");
  function stageLabel(stage) {
    return ({ queued: t("等待连接"), preparing: t("准备连接组件"), components: t("准备连接组件"), contact: t("联系设备"), contacting: t("联系设备"), negotiating: t("正在协商连接"),
      direct: t("正在尝试直连"), relay: t("正在建立中转连接"), verifying: t("验证 SSH 登录"), ssh_verifying: t("验证 SSH 登录"),
      ssh: t("验证 SSH 登录"), ready: t("SSH 已就绪"), failed: t("连接失败"), terminated: t("已终止连接") })[stage] || stage || t("正在连接");
  }
  function blockReason(item) {
    if (!item || item.node_id === localId()) return t("这是本机。");
    if (!localRegistered()) return t("本机尚未完成设备登记，请先处理登记提示。");
    if (testBusy()) return t("连接测试进行中，请等待测试结束。");
    if (incoming().length || device(localId())?.occupancy === 'incoming') return t("本机正被其他设备连接，请先终止连接。");
    if (sessionFor(item.node_id)) return '';
    if (!item.online) return t("设备离线，暂时无法连接。");
    if (isEnding(item)) return t("正在结束连接，确认结束后可重新连接。");
    if (item.occupancy && item.occupancy !== 'idle') return t("已被其他设备连接");
    if (Array.isArray(item.capabilities) && !item.capabilities.includes('ssh_server')) return t("此设备暂不支持接收连接。");
    return '';
  }
  function stateFor(item) {
    if (item.node_id === localId()) {
      if (!localRegistered()) return { text: s()?.last_error ? t("设备未登记") : t("正在登记"), tone: s()?.last_error ? 'bad' : 'warn' };
      if (testBusy()) return { text: test().state === 'cleaning' ? t("正在结束测试") : t("连接测试中"), tone: 'warn' };
      const inbound = incoming()[0];
      if (inbound) return inbound.status === 'connected'
        ? { text: t("已被{0}连接", name(inbound.initiator_node_id)), tone: 'good' }
        : { text: t("{0}正在连接本机", name(inbound.initiator_node_id)), tone: 'warn' };
      if (readySessions().length) return { text: t("已控制{0}台设备", readySessions().length), tone: 'good' };
      const pending = ops().filter(item => workingStates.has(item.state)).length;
      if (outgoing().length || pending) return { text: t("正在连接{0}台设备", Math.max(outgoing().length, pending)), tone: 'warn' };
      if (item.occupancy === 'incoming') return { text: t("正在接收{0}的连接", name(item.occupied_by)), tone: 'warn' };
      return { text: t("在线"), tone: 'good' };
    }
    const session = sessionFor(item.node_id); const op = operation(item.node_id);
    if (session?.target_node_id === localId()) return session.status === 'connected'
      ? { text: t("已连接本机"), tone: 'good' }
      : { text: t("正在连接本机"), tone: 'warn' };
    if (ready(item.node_id)) return { text: t("{0} · 已连接", pathLabel(session.path)), tone: 'good' };
    if (op && workingStates.has(op.state)) return { text: stageLabel(op.stage), tone: 'warn' };
    if (session) return { text: session.status === 'connected' ? t("正在验证 SSH") : t("正在连接"), tone: 'warn' };
    if (!item.online) return { text: t("离线"), tone: 'muted' };
    if (isEnding(item)) return { text: t("正在结束连接"), tone: 'warn' };
    if (item.occupancy && item.occupancy !== 'idle') return { text: t("已被其他设备连接"), tone: 'muted' };
    if (op?.state === 'failed') return { text: t("连接失败"), tone: 'bad' };
    return { text: t("在线"), tone: 'good' };
  }
  function rank(item) {
    if (sessionFor(item.node_id) || workingStates.has(operation(item.node_id)?.state) || isEnding(item)) return 0;
    if (!item.online) return 3;
    return item.occupancy && item.occupancy !== 'idle' ? 2 : 1;
  }
  function testState() {
    const current = test();
    if (['running', 'cleaning'].includes(current.state)) return current.state;
    if (current.state === 'cleanup_failed' || (current.steps || []).some(step => step.id === 'cleanup' && step.state === 'failed')) return 'cleanup_failed';
    if (current.state === 'passed') {
      const required = ['components', 'relay', 'ssh', 'sshd', 'cleanup'];
      if ((current.steps || []).some(step => required.includes(step.id) && step.state === 'failed')) return 'failed';
      if (!required.every(id => (current.steps || []).some(step => step.id === id && step.state === 'passed'))) return 'unverified';
    }
    if (['idle', 'passed', 'failed', 'cancelled', 'unverified'].includes(current.state)) return current.state;
    return 'unverified';
  }
  function testLabel() {
    return ({ idle: t("未进行连接测试"), running: t("连接测试中"), cleaning: t("正在结束测试"), passed: t("连接测试成功"),
      failed: t("连接测试失败"), cleanup_failed: t("连接测试失败 · 清理未完成"), cancelled: t("未完成连接测试"), unverified: t("尚未验证") })[testState()];
  }
  const testTone = () => testState() === 'passed' ? 'good' : ['failed', 'cleanup_failed'].includes(testState()) ? 'bad' : testBusy() ? 'warn' : 'muted';
  function testGate() {
    if (!localRegistered()) return t("本机尚未完成设备登记，请先处理登记提示。");
    if (testBusy()) return '';
    if (hasWork()) return t("请先终止现有连接及连接任务，再开始测试。");
    if (s()?.allow_relay === false || s()?.direct_only === true) return t("当前策略不允许中转，本机连接测试尚未验证。");
    if (device(localId())?.occupancy && device(localId()).occupancy !== 'idle') return t("本机正在参与连接，请结束连接后再测试。");
    return '';
  }
  const isWindows = () => /windows|win32/i.test(String(s()?.platform || device(localId())?.os || ''));
  function field(label, id, value, attrs = {}) {
    return h('label', { class: 'field', 'data-key': `field-${id}`, for: id }, label, h(attrs.multiline ? 'textarea' : 'input', { id, value, ...attrs, multiline: undefined }));
  }
  function deviceNameField(label, id, value, attrs = {}) {
    return h('div', { class: 'field device-name-field', 'data-key': `field-${id}` },
      h('div', { class: 'device-name-heading' }, h('label', { for: id }, label), h('span', { class: 'device-name-support' }, t("支持中文"))),
      h('input', { id, value, ...attrs, 'aria-describedby': `${id}-hint` }),
      h('div', { class: 'device-name-hint', id: `${id}-hint` },
        h('p', {}, t("建议起个好记、易区分的名字，方便与 AI 沟通时指明设备。")),
        h('p', {}, t("例如：开发机、测试机、打包机、宠物喵、侠客剑。"))));
  }
  function metadata(item) {
    return h('dl', { class: 'meta', 'data-key': 'metadata' },
      h('dt', {}, t("系统")), h('dd', {}, item.os || t("未提供")),
      h('dt', {}, t("登录用户")), h('dd', {}, item.ssh_user || (item.node_id === localId() ? s()?.ssh?.login_user : '') || t("待连接后确认")));
  }
  function heading(title, subtitle, item) {
    return h('div', { class: 'detail-head', 'data-key': 'heading' }, h('span', { class: 'glyph' }, icon('monitor')),
      h('div', { class: 'heading-copy' }, h('h1', { tabindex: '-1', 'data-page-heading': '' }, title), h('p', { class: 'muted' }, subtitle)),
      item ? button('', 'menu', { class: 'icon-button', 'aria-label': t("更多设备操作：{0}", item.display_name), 'aria-haspopup': 'menu', 'data-id': item.node_id }, 'more') : null);
  }
  function banner(title, detail, tone = '', copyableDetail = false) {
    return h('div', { class: `banner ${tone}`, 'data-key': 'status-banner' }, h('p', { class: 'banner-title', title }, title), detail ? h('p', { class: 'muted', 'data-copyable': copyableDetail ? '' : undefined }, detail) : null);
  }
  function connectionError(error, stage) {
    const detail = message(error || t("连接未完成，请重试。"));
    if (/REMOTE HOST IDENTIFICATION|Host key verification failed|host.?key.*(?:mismatch|changed)|主机身份.*(?:不符|失败)/i.test(detail)) {
      return { summary: t("设备身份校验未通过，连接已停止，请核实设备身份后重试。"), detail };
    }
    if (detail.length <= 160 && detail.split('\n').length <= 3) return { summary: displayMessage(detail), detail: '' };
    const phase = ({ components: t("连接组件启动失败"), contact: t("设备联系失败"), direct: t("直连未成功"), relay: t("中转连接失败"), ssh: t("SSH 验证失败") })[stage] || t("连接失败");
    let cause;
    if (/permission denied|authentication failed|认证失败|登录失败/i.test(detail)) cause = t("登录权限或凭据未通过验证");
    else if (/timed? ?out|timeout|超时/i.test(detail)) cause = t("连接超时");
    else if (/connection refused|连接被拒绝|拒绝连接/i.test(detail)) cause = t("目标拒绝了连接");
    else if (/no such file|not found|找不到|文件缺失|组件缺失/i.test(detail)) cause = t("所需文件或连接组件缺失");
    else if (/exited|启动后退出|启动失败/i.test(detail)) cause = t("连接组件未能持续运行");
    else cause = detail.split('\n').find(line => line.trim() && !/^[@\s]+$/.test(line))?.trim().slice(0, 90) || t("未能完成连接");
    return { summary: t("{0}：{1}。连接已停止，具体原因见技术详情。", phase, cause), detail };
  }
  function row(item, local = false) {
    const status = stateFor(item); const active = ui.selected === item.node_id && ['device', 'configure'].includes(ui.view);
    return h('button', { type: 'button', class: `device-row${active ? ' active' : ''}`, 'data-key': item.node_id,
      'data-action': 'select-device', 'data-id': item.node_id, 'aria-pressed': active, 'aria-label': `${item.display_name}${local ? t("，本机") : ''}，${status.text}` },
    h('span', { class: 'glyph' }, icon('monitor')), h('span', { class: 'identity' }, h('span', { class: 'device-name', title: item.display_name }, item.display_name),
      h('span', { class: 'device-sub', title: item.purpose || item.os || '' }, `${item.purpose || item.os || t("未填写用途")}${local ? t("（本机）") : ''}`)),
    h('span', { class: `row-status ${status.tone}` }, status.text));
  }
  function sidebar() {
    const all = peers(); const query = ui.query.trim().toLocaleLowerCase();
    const shown = all.filter(item => `${item.display_name || ''} ${item.purpose || ''}`.toLocaleLowerCase().includes(query))
      .sort((a, b) => rank(a) - rank(b) || (a.display_name || '').localeCompare(b.display_name || '', i18n.locale()) || a.node_id.localeCompare(b.node_id));
    return h('aside', { class: 'list-pane', 'aria-label': t("我的设备"), 'data-key': 'list-pane' },
      h('div', { class: 'list-head' }, h('div', { class: 'list-title' }, h('h2', {}, t("我的设备")), h('span', { class: 'muted' }, t("在线 {0} · 离线 {1}", all.filter(item => item.online).length + Number(localRegistered()), all.filter(item => !item.online).length))),
        h('label', { class: 'search' }, icon('search'), h('input', { type: 'search', id: 'device-search', value: ui.query, placeholder: t("搜索名称或用途"), 'aria-label': t("搜索设备名称或用途"), autocomplete: 'off' }))),
      h('div', { class: 'local-block' }, row(device(localId()), true), button(t("连接测试 · {0}", testLabel()), 'test', { class: `local-test ${testTone()}`, 'aria-label': t("连接测试，{0}", testLabel()) })),
      h('div', { class: 'device-list', 'data-key': 'device-list', 'aria-label': t("其他设备") }, shown.length ? shown.map(item => row(item)) : h('div', { class: 'empty' }, query ? t("没有匹配的设备") : t("登录其他电脑后，设备会显示在这里。"))),
      h('div', { class: 'nav-all-area' }, button([
        h('span', { class: 'nav-all-icon' }, icon('grid')),
        h('span', { class: 'identity' }, h('span', { class: 'nav-all-title' }, t("连接所有设备")), h('span', { class: 'muted' }, t("配置连接方式，查看进度与结果"))),
        icon('arrow'),
      ], 'all', { class: `nav-all${ui.view === 'all' ? ' active' : ''}`, 'aria-current': ui.view === 'all' ? 'page' : undefined })));
  }
  function draftFor(item) {
    return ui.drafts.get(item.node_id) || { display_name: item.display_name || '', purpose: item.purpose || '' };
  }
  function editForm(item, local) {
    const draft = draftFor(item);
    return h('form', { 'data-form': 'metadata', 'data-id': item.node_id, 'data-key': `edit-${item.node_id}` },
      local ? deviceNameField(t("本机名称"), 'device-display-name', draft.display_name, { name: 'display_name', maxlength: 80, required: true, 'data-draft': item.node_id }) : null,
      field(t("用途备注"), 'device-purpose', draft.purpose, { name: 'purpose', maxlength: 500, placeholder: t("例如：开发环境、文件整理"), 'data-draft': item.node_id, multiline: true }),
      h('div', { class: 'actions device-edit-actions' },
        button(ui.busy.has('metadata') ? t("正在保存…") : t("保存修改"), '', { type: 'submit', disabled: !localRegistered() || ui.busy.has('metadata') || !ui.drafts.has(item.node_id) }),
        button(t("删除设备"), 'delete-device', { class: 'button danger', 'data-id': item.node_id, disabled: !localRegistered() || ui.busy.has('delete-device') })));
  }
  function devicePage() {
    const item = device(ui.selected); if (!item) return h('div', { class: 'empty' }, t("这台设备已不在账号列表中。"));
    const local = item.node_id === localId(); const status = stateFor(item); const session = sessionFor(item.node_id); const op = operation(item.node_id);
    const working = workingStates.has(op?.state); const canEnd = !!session || working;
    const failure = op?.state === 'failed' ? connectionError(op.message, op.stage) : null;
    const stateDetail = local ? incoming().length ? t("本机当前正在接收连接，结束后可以发起新的连接。") : t("连接信息仅在当前电脑和当前会话有效。")
      : ready(item.node_id) ? t("已连接{0}。SSH 登录已验证，可复制命令。", item.display_name)
      : failure ? failure.summary
      : blockReason(item) || (session ? displayMessage(op?.message || t("网络通道建立后，还需要验证 SSH 登录。")) : t("设备在线，当前空闲。点击连接后可选择连接方式。"));
    return h('div', { 'data-key': `device-page-${item.node_id}` },
      ui.failureGroup ? h('div', { class: 'backline' }, button(t("返回连接结果"), 'all', { class: 'link' }, 'back'),
        h('div', { class: 'actions' }, button(t("上一台"), 'failure-prev', { class: 'link' }), button(t("下一台同类设备"), 'failure-next', { class: 'link' }))) : null,
      heading(local ? t("本机信息") : item.display_name, local ? t("这台电脑") : item.purpose || t("设备详情"), item),
      banner(status.text, stateDetail, status.tone === 'good' ? 'good' : status.tone === 'bad' ? 'bad' : '', !!failure),
      failure?.detail ? h('details', { class: 'details', 'data-key': 'connection-error-details' }, h('summary', {}, t("技术详情")),
        h('pre', { class: 'command', 'data-copyable': '' }, failure.detail)) : null,
      local ? button(h('span', { class: 'identity' }, h('strong', {}, t("连接测试")), h('p', { class: testTone() }, testLabel())), 'test', {
        class: 'test-entry', 'data-key': 'local-test-entry', title: test().finished_at ? t("最近一次测试：{0}", formatTime(test().finished_at)) : t("检查本机能否正常发起和接收连接"),
      }, 'test') : null,
      local ? h('div', { class: 'local-actions actions', 'data-key': 'local-actions' },
        incoming().length ? button(t("终止连接"), 'terminate-one', { class: 'button danger', 'data-id': incoming()[0].initiator_node_id, disabled: ui.busy.has('terminate') }) : null,
        outgoing().length ? button(t("管理本机连接"), 'all', { class: 'link' }) : null) :
      h('div', { class: 'actions stretch', 'data-key': 'connection-actions' },
        canEnd ? button(t("终止连接"), 'terminate-one', { class: 'button danger', 'data-id': item.node_id, disabled: ui.busy.has('terminate') }) : button(t("连接设备"), 'configure', { class: 'button primary', 'data-id': item.node_id, disabled: !!blockReason(item) || ui.busy.has('connect') }),
        ready(item.node_id) ? button(t("复制 SSH"), 'copy-ssh', { 'data-id': item.node_id, disabled: !!ui.syncError }, 'copy') : null,
        ready(item.node_id) ? button(t("复制给 AI"), 'export-one', { 'data-id': item.node_id, disabled: !!ui.syncError }) : null),
      ready(item.node_id) && !ui.syncError ? h('pre', { class: 'command', 'data-key': 'ssh-command', 'aria-label': t("当前 SSH 命令"), 'data-copyable': '' }, session.ssh_command) : null,
      h('div', { class: 'section', 'data-key': 'device-fields' }, editForm(item, local), metadata(item)));
  }
  function modeOptions(disabled = false) {
    const choices = [
      ['direct_preferred', t("自动连接"), t("兼容性优先"), t("优先尝试设备直连；无法直连时，连接数据通过中转服务传输。信息敏感环境，请选择「仅限直连」。")],
      ['direct_only', t("仅限直连"), t("点对点传输"), t("连接数据在设备间直接传输，不经中转服务。部分网络环境可能无法连接；失败后不会切换中转。")],
    ];
    return h('div', { class: 'mode-options', role: 'radiogroup', 'aria-label': t("连接方式"), 'data-key': 'mode-options' }, choices.map(([value, label, subtitle, copy]) =>
      h('label', { class: `mode-option${ui.policy === value ? ' chosen' : ''}`, 'data-key': value }, h('input', { type: 'radio', name: 'connection-policy', value, checked: ui.policy === value, disabled }),
        h('span', {}, h('strong', {}, label), h('strong', { class: 'muted' }, subtitle), copy))));
  }
  function plan() {
    const result = { fresh: 0, reuse: 0, skip: 0, relay: 0, rebuild: 0, offline: 0, occupied: 0, ending: 0, unsupported: 0 };
    for (const item of peers()) {
      const session = sessionFor(item.node_id);
      if (session?.initiator_node_id === localId()) {
        if (ui.policy === 'direct_only' && session.path === 'relay') { result.relay++; if (ui.rebuild) result.rebuild++; }
        else result.reuse++;
      } else if (!item.online) { result.skip++; result.offline++; }
      else if (blockReason(item)) {
        result.skip++;
        if (isEnding(item)) result.ending++;
        else if (item.occupancy && item.occupancy !== 'idle') result.occupied++;
        else result.unsupported++;
      }
      else result.fresh++;
    }
    return result;
  }
  function summaryLine(label, value) { return h('div', { class: 'summary-line' }, h('span', {}, label), h('strong', {}, value)); }
  function allConnectionActions() {
    if (!hasOutgoingWork() && !readySessions().length) return null;
    return h('div', { class: 'bottom', 'data-key': 'existing-management' }, h('div', { class: 'section-title' }, t("当前本机连接")),
      h('div', { class: 'actions stretch' }, button(t("终止全部连接"), 'terminate-all', { class: 'button danger', disabled: ui.busy.has('terminate') }),
        button(t("复制全部给 AI{0}", readySessions().length ? `（${readySessions().length}）` : ''), 'export-all', { disabled: !readySessions().length || !!ui.syncError })),
      h('p', { class: 'muted' }, t("终止包括已有连接、正在建立的连接和等待任务；复制仅包含已验证 SSH 的连接。")));
  }
  function allPage() {
    const batch = s()?.batch;
    if (batch && !ui.allConfig) return batchPage(batch);
    const counts = plan(); const locked = !localRegistered() || testBusy() || incoming().length > 0 || device(localId())?.occupancy === 'incoming';
    return h('div', { 'data-key': 'all-config' }, heading(t("连接所有设备"), t("账号下除本机外的全部 {0} 台设备，搜索不会改变连接范围。", peers().length)),
      modeOptions(!!batch?.running),
      h('p', { class: 'muted' }, t("两种模式都需要登录和连接协商。仅限直连限制连接数据路径。")),
      ui.policy === 'direct_only' && counts.relay > 0 ? h('div', { class: 'section', 'data-key': 'relay-choice' }, h('h3', {}, t("已有 {0} 台使用中转连接", counts.relay)),
        h('label', { class: 'choice' }, h('input', { type: 'radio', name: 'relay-choice', value: 'keep', checked: !ui.rebuild }), t("保留现有中转，不计入本次直连成功")),
        h('label', { class: 'choice' }, h('input', { type: 'radio', name: 'relay-choice', value: 'rebuild', checked: ui.rebuild }), t("断开这些连接，并重新尝试直连"))) : null,
      h('div', { class: 'config-summary' }, summaryLine(t("新建连接"), t("{0} 台", counts.fresh + counts.rebuild)), summaryLine(t("复用当前连接"), t("{0} 台", counts.reuse)),
        counts.relay && !ui.rebuild ? summaryLine(t("保留现有中转"), t("{0} 台（不计本次直连成功）", counts.relay)) : null,
        summaryLine(t("跳过设备"), t("{0} 台", counts.skip)), counts.skip ? h('p', { class: 'muted' }, t("离线 {0} · 已被其他设备连接 {1}{2} · 其他不可连接 {3}", counts.offline, counts.occupied, counts.ending ? t(" · 正在结束连接 {0}", counts.ending) : '', counts.unsupported)) : null),
      h('div', { class: 'bottom' }, ui.policy === 'direct_only' && ui.rebuild && counts.relay ? banner(t("现有 SSH 会中断"), t("点击开始后会断开已有中转连接。直连失败不回退，请确认相关工作已保存。"), 'warn') : null,
        locked ? h('p', { class: 'warn small' }, !localRegistered() ? t("本机尚未完成设备登记，请先处理登记提示。") : testBusy() ? t("连接测试进行中，结束后可以连接设备。") : t("本机正在接收连接，请先终止该连接。")) : null,
        button(batch?.running ? t("查看连接进度") : ui.busy.has('connect-all') ? t("正在开始…") : t("开始连接"), batch?.running ? 'all-progress' : 'start-all', { class: 'button primary wide', disabled: !batch?.running && (locked || !peers().length || ui.busy.has('connect-all')) })),
      allConnectionActions());
  }
  function category(item) {
    const known = { components: t("设备联系失败"), contact: t("设备联系失败"), direct: t("直连未成功"), relay: t("中转连接失败"), ssh: t("SSH 验证失败") };
    if (known[item.stage]) return known[item.stage];
    const source = `${item.stage || ''} ${item.message || ''}`.toLowerCase();
    if (/ssh|登录验证/.test(source)) return t("SSH 验证失败");
    if (/relay|中转/.test(source)) return t("中转连接失败");
    if (/direct|直连/.test(source)) return t("直连未成功");
    return t("设备联系失败");
  }
  function failures() {
    const groups = new Map();
    for (const item of s()?.batch?.items || []) if (item.state === 'failed') {
      const label = category(item); if (!groups.has(label)) groups.set(label, []); groups.get(label).push(item.node_id);
    }
    return groups;
  }
  function batchPage(batch) {
    const items = batch.items || []; const completed = Number(batch.completed || 0); const total = Number(batch.total || 0);
    const live = readySessions(); const retained = new Set(items.filter(item => item.state === 'retained_relay').map(item => item.node_id));
    const direct = live.filter(item => item.path === 'direct').length;
    const relay = live.filter(item => item.path === 'relay' && !retained.has(item.target_node_id)).length;
    const failed = items.filter(item => item.state === 'failed').length;
    const retainedCount = live.filter(item => retained.has(item.target_node_id)).length;
    return h('div', { 'data-key': 'all-progress' }, heading(t("连接所有设备"), batch.running ? t("连接正在进行，逐台状态显示在左侧设备列表。") : t("本次连接已结束。")),
      h('div', { class: 'progress-line' }, h('span', {}, batch.running ? t("正在处理") : t("已处理")), h('span', {}, t("{0} / {1} 台", Math.min(completed, total), total))),
      h('progress', { max: Math.max(total, 1), value: Math.min(completed, total), 'aria-label': t("已处理设备数") }),
      h('div', { class: 'metrics' }, metric(direct, t("当前直连就绪")), metric(relay, t("当前中转就绪")), metric(failed, t("本次失败"))),
      h('div', { class: 'config-summary' }, summaryLine(t("跳过"), t("{0} 台", items.filter(item => item.state === 'skipped').length)),
        summaryLine(t("已终止"), t("{0} 台", items.filter(item => item.state === 'terminated').length)),
        retainedCount ? summaryLine(t("保留的中转"), t("{0} 台（不计本次直连成功）", retainedCount)) : null),
      failed ? h('section', { class: 'section', 'aria-label': t("失败分类") }, h('h3', {}, t("失败原因")), [...failures()].map(([label, ids]) =>
        button([label, h('span', {}, t("{0} 台", ids.length), icon('arrow'))], 'failure-category', { class: 'failure-row', 'data-category': label, 'aria-label': t("{0}，{1}台，查看设备", label, ids.length) }))) : null,
      !batch.running ? h('div', { class: 'bottom' }, button(t("重新配置连接"), 'all-config', { class: 'button wide' })) : null,
      allConnectionActions());
  }
  function metric(value, label) { return h('div', { class: 'metric' }, h('strong', {}, value), h('span', {}, label)); }
  function configurePage() {
    const item = device(ui.selected); if (!item) return h('p', {}, t("设备已不在账号列表中。"));
    const existing = sessionFor(item.node_id); const reason = blockReason(item);
    return h('div', { 'data-key': `configure-${item.node_id}` }, h('div', { class: 'backline' }, button(t("返回设备详情"), 'device', { class: 'link' }, 'back')),
      heading(t("连接{0}", item.display_name), item.purpose || item.os || t("选择连接方式后再开始。")),
      existing ? banner(t("将复用现有连接"), t("重复连接不会中断已有会话。如需更换连接方式，请先终止现有连接。")) : modeOptions(),
      h('p', { class: 'muted' }, t("两种模式都需要登录和连接协商。仅限直连限制连接数据路径。")),
      reason ? h('div', { class: 'section warn' }, reason) : null,
      h('div', { class: 'bottom' }, button(ui.busy.has('connect') ? t("正在开始…") : existing ? t("返回当前连接") : t("开始连接"), existing ? 'device' : 'start-one', { class: 'button primary wide', 'data-id': item.node_id, disabled: !!reason || ui.busy.has('connect') })));
  }
  function formatTime(value) {
    if (!value) return '';
    const date = new Date(typeof value === 'number' ? (value < 1e12 ? value * 1000 : value) : value);
    return Number.isNaN(date.getTime()) ? '' : date.toLocaleString(i18n.locale(), { hour12: false });
  }
  function historyPage() {
    const loading = ui.busy.has('history');
    const statusLabels = { negotiating: t("协商中"), connected: t("已建立"), closed: t("已结束"), failed: t("连接失败") };
    const filtered = ui.historyItems.filter(item => (ui.historyDirection === 'all' || item.direction === ui.historyDirection)
      && (!ui.historyFailed || item.status === 'failed'));
    const items = ui.historyAscending ? [...filtered].reverse() : filtered;
    const localName = device(localId())?.display_name || t("这台电脑");
    const connectionPath = item => ({ direct: t("直连"), relay: t("中转") })[item.path] || t("方式未确定");
    const rows = items.flatMap(item => {
      const peer = item.peer_name || t("已删除的设备"); const incoming = item.direction === 'incoming';
      const expanded = ui.historyExpanded === item.session_id;
      const detailsId = `history-details-${encodeURIComponent(item.session_id)}`;
      return [h('tr', { 'data-key': `history-row-${item.session_id}`, class: expanded ? 'history-selected' : '' },
        h('td', { class: 'history-time' }, formatTime(item.created_at) || t("未记录")),
        h('td', { class: incoming ? '' : 'history-local', title: incoming ? peer : localName }, incoming ? peer : t("本机")),
        h('td', { class: 'history-arrow', 'aria-label': t("连接至") }, '→'),
        h('td', { class: incoming ? 'history-local' : '', title: incoming ? localName : peer }, incoming ? t("本机") : peer),
        h('td', {}, connectionPath(item)),
        h('td', {}, h('span', { class: `history-status${item.status === 'failed' ? ' bad' : item.status === 'connected' ? ' good' : ''}` }, statusLabels[item.status] || t("状态未知"))),
        h('td', { class: 'history-action' }, button(expanded ? t("收起") : item.failure_reason ? t("查看原因") : t("详情"), 'history-details',
          { class: 'link', 'data-session': item.session_id, 'aria-expanded': String(expanded), 'aria-controls': detailsId,
            'aria-label': t("{0}{1}的连接记录详情", expanded ? t("收起") : t("查看"), peer) }))),
      h('tr', { 'data-key': `history-details-${item.session_id}`, hidden: !expanded }, h('td', { colspan: '7', class: 'history-detail-cell' },
        h('div', { id: detailsId, class: 'history-details' },
          h('p', {}, `${incoming ? t("{0} → 本机", peer) : t("本机 → {0}", peer)} · ${connectionPath(item)} · ${statusLabels[item.status] || t("状态未知")}`),
          h('p', { class: 'muted' }, t("发起时间：{0}", formatTime(item.created_at) || t("未记录"))),
          item.failure_reason ? h('p', { class: 'history-reason', 'data-copyable': '' }, displayMessage(item.failure_reason)) : null)))];
    });
    const count = ui.historyLoaded ? t("显示 {0} 条 · 已加载 {1} / {2} 条", items.length, ui.historyItems.length, ui.historyTotal) : loading ? t("正在读取连接记录…") : t("尚未读取连接记录");
    return h('main', { class: 'history-page', 'data-key': 'history-page', 'aria-label': t("这台电脑的连接记录") },
      h('div', { class: 'history-top', 'data-key': 'history-top' }, button(t("返回设备"), 'device', { class: 'link' }, 'back'),
        button(loading ? t("正在读取…") : t("刷新记录"), 'history-refresh', { disabled: loading }, 'refresh')),
      heading(t("这台电脑的连接记录"), t("{0}（本机）参与的全部连接，包含本机发起和接收的连接。", localName)),
      h('div', { class: 'history-toolbar', 'data-key': 'history-toolbar' },
        h('div', { class: 'history-directions', role: 'group', 'aria-label': t("连接方向") },
          [['all', t("全部")], ['outgoing', t("我发起的")], ['incoming', t("连接我的")]].map(([value, label]) =>
            button(label, 'history-direction', { class: 'history-filter', 'data-direction': value, 'aria-pressed': String(ui.historyDirection === value) }))),
        h('label', { class: 'history-failed' }, h('input', { id: 'history-failed', type: 'checkbox', checked: ui.historyFailed }), t("只看失败")),
        h('span', { class: 'muted' }, t("筛选与排序仅针对已加载记录"))),
      h('div', { 'data-key': 'history-notices' }, registrationNotice(), rebindNotice()),
      ui.historyError ? h('div', { 'data-key': 'history-error' }, banner(t("连接记录读取失败"), displayMessage(ui.historyError), 'warn', true)) : null,
      h('div', { class: 'history-records', 'data-key': 'history-records' },
        items.length ? h('table', { class: 'history-table', 'aria-label': t("本机参与的连接记录") },
          h('thead', {}, h('tr', {},
            h('th', { scope: 'col', 'aria-sort': ui.historyAscending ? 'ascending' : 'descending' },
              button(t("发起时间 {0}", ui.historyAscending ? '↑' : '↓'), 'history-sort', { class: 'link', 'aria-label': t("发起时间，当前从{0}排列", ui.historyAscending ? t("远到近") : t("近到远")) })),
            h('th', { scope: 'col' }, t("发起设备")), h('th', { scope: 'col', class: 'history-arrow', 'aria-label': t("连接方向") }),
            h('th', { scope: 'col' }, t("接收设备")), h('th', { scope: 'col' }, t("方式")), h('th', { scope: 'col' }, t("记录状态")), h('th', { scope: 'col', class: 'history-action' }, t("详情")))),
          h('tbody', {}, rows))
          : h('p', { class: 'empty', role: 'status' }, loading ? t("正在读取连接记录…") : ui.historyError ? t("记录暂时无法读取，请刷新重试。")
            : !ui.historyLoaded ? t("尚未读取连接记录。") : ui.historyItems.length ? t("已加载记录中没有符合条件的记录。") : t("这台电脑还没有连接记录。"))),
      h('div', { class: 'history-bottom', 'data-key': 'history-bottom' }, h('span', { class: 'muted', 'aria-live': 'polite' }, count),
        ui.historyLoaded && ui.historyOffset < ui.historyTotal ? button(loading ? t("正在读取…") : t("加载更多"), 'history-more', { disabled: loading }) : null),
      h('p', { class: 'muted history-note', 'data-key': 'history-note' }, t("状态为最近一次读取的记录；当前连接请返回设备查看。")));
  }
  function resetHistory() {
    ++ui.historyRequest; ui.historyItems = []; ui.historyTotal = 0; ui.historyOffset = 0;
    ui.historyLoaded = false; ui.historyError = ''; ui.busy.delete('history');
    ui.historyDirection = 'all'; ui.historyFailed = false; ui.historyAscending = false; ui.historyExpanded = null;
  }
  async function loadHistory(append = false) {
    if (ui.busy.has('history') || !s()?.logged_in) return;
    const request = ++ui.historyRequest; const offset = append ? ui.historyOffset : 0;
    ui.busy.add('history'); ui.historyError = ''; render();
    try {
      const result = await api('client_connection_history', { form: { offset } });
      if (request !== ui.historyRequest) return;
      const items = result?.items;
      if (!Array.isArray(items) || !Number.isSafeInteger(result.total) || result.total < 0) throw new Error(t("连接记录响应无法读取，请稍后重试。"));
      const combined = append ? [...ui.historyItems, ...items] : items;
      ui.historyItems = [...new Map(combined.map(item => [item.session_id, item])).values()];
      ui.historyTotal = result.total; ui.historyOffset = offset + items.length; ui.historyLoaded = true;
    } catch (error) { if (request === ui.historyRequest) ui.historyError = message(error); }
    finally { if (request === ui.historyRequest) { ui.busy.delete('history'); render(); } }
  }
  function testLinks() {
    return h('div', { class: 'actions', 'data-key': 'test-links' }, button(t("组件来源与校验"), 'components', { class: 'link' }),
      isWindows() ? button(t("Windows 处理方法"), 'windows-help', { class: 'link' }) : null);
  }
  function testExplanation() {
    return h('div', { class: 'test-copy', 'data-key': 'test-explanation' },
      h('p', {}, t("检查这台电脑是否可以正常发起和接收连接。")), h('h3', {}, t("连接组件与安全软件提示")),
      h('p', {}, t("我们使用 FRP 官方发布的连接组件，打包时不修改程序本体。部分安全软件可能将其误报或隔离，导致连接失败。")),
      h('p', {}, t("测试过程中可能出现防火墙提示。请核对程序名称和路径，并在防火墙中允许 StarBridge 及其连接组件进行网络通信。")),
      h('p', {}, t("如果提示风险，请先核对程序来源及官方文件校验结果。确认属于误报后，再按安全软件提示放行相关文件。")),
      h('div', { class: 'section' }, testLinks()));
  }
  function testPage() {
    const current = test(); const gate = testGate(); const initial = testState() === 'idle';
    const summary = testState() === 'cleanup_failed' ? (current.steps || []).find(step => step.id === 'cleanup')?.message || t("测试资源清理未完成，不能显示成功。")
      : current.state === 'passed' && testState() !== 'passed' ? t("缺少完整的验证结果，尚不能确认本机连接测试通过。")
      : current.message || stageLabel(current.stage);
    const stepText = { pending: t("待执行"), running: t("进行中"), passed: t("通过"), failed: t("未通过"), skipped: t("未执行"), unverified: t("尚未验证"), cancelled: t("已取消") };
    return h('div', { 'data-key': 'test-page' }, heading(t("本机连接测试"), initial ? t("测试需要本机空闲，并允许使用中转。") : t("显示最近一次测试状态。")),
      initial ? testExplanation() : h('div', { 'data-key': 'test-result' }, banner(testLabel(), displayMessage(summary), testTone(), ['failed', 'cleanup_failed'].includes(testState())),
        (current.steps || []).map(step => h('div', { class: 'stage-row', 'data-key': step.id }, icon(step.state === 'passed' ? 'check' : step.state === 'failed' ? 'close' : 'test'),
          h('div', {}, i18n.message(step.label), step.message ? h('p', { 'data-copyable': step.state === 'failed' ? '' : undefined }, displayMessage(step.message)) : null), h('span', { class: step.state === 'passed' ? 'good small' : step.state === 'failed' ? 'bad small' : 'muted' }, stepText[step.state] || t("尚未验证")))),
        current.finished_at ? h('p', { class: 'muted section' }, t("最近一次测试：{0}", formatTime(current.finished_at))) : null,
        h('details', { class: 'details', 'data-key': 'test-details' }, h('summary', {}, t("测试范围与技术详情")),
          h('p', {}, t("使用正式连接组件及隔离的临时 SSH 配置，经中转真实连接回本机，验证登录和固定数据回传。完成清理后才显示最终结果。")),
          h('p', {}, t("本测试默认不验证跨设备直连。文件传输未测试。网络超时不代表安全软件已拦截。")),
          current.id ? h('p', {}, t("测试 ID："), h('span', { 'data-copyable': '' }, current.id)) : null), testLinks()),
      h('div', { class: 'bottom' }, gate ? h('p', { class: 'warn small' }, gate) : null,
        current.state === 'cleaning' ? button(t("正在清理测试连接…"), '', { class: 'button wide', disabled: true })
          : current.state === 'running' ? button(t("取消测试"), 'cancel-test', { class: 'button danger wide', disabled: ui.busy.has('cancel-test') })
          : button(ui.busy.has('start-test') ? t("正在开始…") : initial ? t("开始连接测试") : t("重新测试"), 'start-test', { class: 'button primary wide', disabled: !!gate || ui.busy.has('start-test') }),
        !initial && !testBusy() ? button(t("查看测试说明"), 'test-explanation', { class: 'link' }) : null));
  }
  function componentsPage() {
    const info = ui.components;
    return h('div', { 'data-key': 'components-page' }, h('div', { class: 'backline' }, button(t("返回连接测试"), 'test', { class: 'link' }, 'back')),
      heading(t("组件来源与校验"), t("官方发布地址和本机实际文件信息。")),
      !info ? h('p', { class: 'muted' }, t("正在读取组件信息…")) : h('div', {},
        info.version ? h('p', {}, t("客户端版本：{0}", info.version)) : null,
        sourceLink(t("FRP 官方发布"), info.official_url), sourceLink(t("官方校验文件"), info.checksums_url),
        h('p', { class: 'muted section' }, t("仅提供下载来源不代表本机文件已通过校验。只有明确比对官方校验值后才显示通过。")),
        (info.files || []).map((file, index) => h('div', { class: 'source-file', 'data-key': `file-${index}` }, h('h3', {}, file.name),
          file.version ? h('p', { class: 'muted' }, t("组件版本：{0}", file.version)) : null,
          h('p', { class: file.verification === 'verified' ? 'good' : ['missing', 'mismatch'].includes(file.verification) ? 'bad' : 'warn' },
            ({ verified: t("与发布清单的校验值一致"), missing: t("文件缺失"), mismatch: t("与发布清单不一致"), not_verified: t("尚未完成来源校验") })[file.verification] || t("尚未完成来源校验")),
          file.path ? h('p', {}, t("本机路径："), h('span', { 'data-copyable': '' }, file.path)) : null, file.sha256 ? h('p', {}, 'SHA-256：', h('span', { 'data-copyable': '' }, file.sha256)) : null,
          file.message ? h('p', { class: 'muted', 'data-copyable': '' }, displayMessage(file.message)) : null,
          file.source_url ? sourceLink(t("此文件的发布来源"), file.source_url) : null)),
        isWindows() ? h('div', { class: 'section' }, button(t("Windows 处理方法"), 'windows-help', { class: 'link' })) : null));
  }
  function sourceLink(label, url) {
    if (!url) return null;
    return h('div', { class: 'section' }, h('h3', {}, label), h('p', { class: 'command', 'data-copyable': '' }, url), button(t("复制地址"), 'copy-url', { class: 'link', 'data-url': url }, 'copy'));
  }
  function fingerprint(ids = ui.exportIds) {
    return readySessions().filter(session => !ids.length || ids.includes(session.target_node_id)).map(session => `${session.session_id}:${session.verified_at || ''}`).sort().join('|');
  }
  function exportText(data) {
    if (!data) return '';
    const privilege = { user: t('普通用户权限，执行可能受限'), admin: t('管理员权限'), system: t('SYSTEM 权限'), unknown: t('权限未知') };
    const lines = (data.items || []).map(item => t('设备：{0}\n用途：{1}\n系统：{2}\n登录用户：{3}（{4}）\n路径：{5}\n来源电脑：{6}\nShell：{7}\nSSH：{8}\n有效期：{9}',
      item.name, item.purpose || t('未设置'), item.system, item.ssh_user, privilege[item.privilege] || t('权限未知'),
      pathLabel(item.path), item.source_name, item.shell, item.ssh_command, t('仅适用于发起连接的本机与当前会话；终止、退出或服务重启后请重新获取')));
    if (data.excluded?.length) lines.push(t('未包含的设备：') + '\n' + data.excluded.map(item => `- ${item.name}: ${displayMessage(item.reason)}`).join('\n'));
    return lines.join('\n\n');
  }
  function exportPage() {
    const currentFingerprint = fingerprint(); const changed = !currentFingerprint || currentFingerprint !== ui.exportFingerprint; const data = ui.exportData;
    return h('div', { 'data-key': 'export-page' }, h('div', { class: 'backline' }, button(t("返回"), 'export-back', { class: 'link' }, 'back')),
      heading(t("复制连接信息给 AI"), t("仅包含当前已验证且仍活动的 SSH 连接。")),
      changed || ui.syncError ? banner(t("连接状态已变化"), t("复制前会重新读取有效连接。已结束的会话命令不再可用。"), 'warn') : null,
      h('textarea', { class: 'export-text', readOnly: true, value: changed || ui.syncError ? t("连接状态已变化，请重新获取连接信息。") : exportText(data) || t("正在生成连接信息…"), 'aria-label': t("将复制的连接信息"), 'data-key': 'export-preview', 'data-copyable': !changed && !ui.syncError && data?.text ? '' : undefined }),
      h('p', { class: 'muted' }, t("这些命令只适用于发起连接的本机。断开、退出或服务重启后，请重新获取。")),
      h('div', { class: 'bottom actions stretch' }, button(changed ? t("重新获取") : t("刷新预览"), 'refresh-export', { disabled: ui.busy.has('export') || !!ui.syncError }),
        button(t("复制给 AI"), 'copy-export', { class: 'button primary', disabled: ui.busy.has('export') || !!ui.syncError || !currentFingerprint }, 'copy')));
  }
  function renderDetail() {
    return h('div', {}, registrationNotice(), rebindNotice(), ({ device: devicePage, configure: configurePage, all: allPage, test: testPage, components: componentsPage, export: exportPage })[ui.view]?.() || devicePage());
  }
  function registrationNotice() {
    if (localRegistered() || s()?.device_rebind_required) return null;
    return h('div', { class: 'banner warn', 'data-key': 'device-registration-notice', role: 'alert' },
      h('p', { class: 'banner-title' }, t("账号已登录，本机尚未完成登记")),
      h('p', { 'data-copyable': s()?.last_error ? '' : undefined }, s()?.last_error ? displayMessage(s().last_error) : t("正在登记本机，完成后才能修改设备信息和建立连接。")));
  }
  function rebindNotice() {
    if (!s()?.device_rebind_required) return null;
    return h('div', { class: 'banner warn', 'data-key': 'device-rebind-notice' },
      h('p', { class: 'banner-title' }, t("这台电脑需要确认重新绑定")),
      h('p', {}, t("现有设备凭据无法复用。确认账号后，可替换原凭据；旧客户端将失去访问权限。")),
      button(t("重新绑定这台电脑"), 'rebind-device', { class: 'button', disabled: ui.busy.has('rebind-device') }));
  }
  function header(loggedIn) {
    return h('header', { class: 'app-header', 'data-key': 'app-header' }, h('span', { class: 'brand-mark', 'aria-hidden': 'true' }, 'S'), h('span', {}, 'StarBridge'),
      h('div', { class: 'account' }, button([h('span', { class: i18n.locale() === 'zh-CN' ? 'language-current' : '' }, '中'),
        h('span', { class: 'language-divider' }, '/'), h('span', { class: i18n.locale() === 'en' ? 'language-current' : '' }, 'EN'), icon('down')], 'language-menu',
        { id: 'language-trigger', class: 'language-trigger', 'data-key': 'language-trigger', 'aria-label': '语言 / Language', 'aria-haspopup': 'menu', 'aria-expanded': String(ui.languageOpen), 'aria-controls': 'language-menu' }, 'globe'),
        loggedIn ? [h('span', { class: 'account-name', title: s().username || '' }, s().username || ''),
        button(t("连接记录"), 'history', { class: 'link', 'aria-current': ui.view === 'history' ? 'page' : undefined }, 'history'),
        button(t("退出登录"), 'logout', { class: 'link', disabled: ui.busy.has('logout') })] : null),
      frameless ? h('div', { class: 'window-controls', 'aria-label': t("窗口操作") },
        button('', 'window-minimize', { class: 'window-button', 'aria-label': t("最小化窗口"), title: t("最小化") }, 'minimize'),
        button('', 'window-close', { class: 'window-button window-close', 'aria-label': t(closeLabel), title: t(closeLabel) }, 'close')) : null);
  }
  function authPage() {
    const registering = ui.authMode === 'register';
    return h('div', { class: 'shell', 'data-key': 'auth-shell' }, header(false), h('main', { class: 'auth' },
      h('form', { class: 'auth-form', 'data-form': 'auth' }, h('h1', {}, registering ? t("创建账号") : t("登录账号")), h('p', { class: 'intro' }, t("登录后，在你的电脑之间建立连接。")),
        field(t("用户名"), 'auth-username', ui.auth.username, { name: 'username', autocomplete: 'username', required: true, maxlength: 80 }),
        field(t("密码"), 'auth-password', ui.auth.password, { name: 'password', type: 'password', autocomplete: registering ? 'new-password' : 'current-password', required: true, minlength: 4 }),
        registering ? field(t("确认密码"), 'auth-confirm', ui.auth.confirm_password, { name: 'confirm_password', type: 'password', autocomplete: 'new-password', required: true, minlength: 4 }) : null,
        h('label', { class: 'remember', 'data-key': 'remember' }, h('input', { id: 'auth-remember', type: 'checkbox', name: 'remember', checked: ui.auth.remember }), t("记住登录状态")),
        ui.authError ? h('p', { class: 'auth-error', role: 'alert', 'data-key': 'auth-error', 'data-copyable': '' }, displayMessage(ui.authError)) : null,
        rebindNotice(),
        button(ui.busy.has('auth') ? t("正在处理…") : registering ? t("注册并登录") : t("登录"), '', { type: 'submit', class: 'button primary wide', disabled: ui.busy.has('auth') }),
        h('div', { class: 'auth-switch' }, button(registering ? t("已有账号，返回登录") : t("没有账号？注册"), 'auth-switch', { class: 'link', disabled: ui.busy.has('auth') })))));
  }
  function render() {
    renderLanguageMenu();
    if (!s()) {
      patchChildren(root, [h('div', { class: 'shell', 'data-key': 'startup-shell' }, header(false),
        h('main', { class: 'startup', role: 'status' }, ui.loadError ? h('span', { 'data-copyable': '' }, displayMessage(ui.loadError)) : t("正在读取登录状态…"), ui.loadError ? button(t("重试"), 'retry') : null))]);
      return;
    }
    root.setAttribute('aria-busy', 'false');
    if (!s().logged_in) { patchChildren(root, [authPage()]); return; }
    const content = h('div', { class: 'shell', 'data-key': 'main-shell' }, header(true), ui.view === 'history' ? historyPage() : h('div', { class: 'workspace', 'data-key': 'device-workspace' }, sidebar(),
      h('main', { class: `detail${ui.view === 'device' && ui.selected === localId() ? ' local-detail' : ''}`, 'data-key': `detail-${ui.view}-${ui.selected || ''}`, 'aria-label': t("设备详情与操作") }, renderDetail())),
      h('footer', { class: 'footbar' }, h('span', { class: !localRegistered() || ui.syncError || s().stage === 'heartbeat_failed' ? 'bad' : '', title: s().stage === 'heartbeat_failed' ? t("暂时无法更新账号设备状态，请检查网络连接。") : undefined },
        !localRegistered() ? t("本机尚未登记，设备同步与连接暂不可用。") : ui.syncError ? t("状态更新失败，正在重试；复制操作暂不可用。") : s().stage === 'heartbeat_failed' ? t("暂时无法更新账号设备状态，正在重试。") : t("在线设备 {0} · SSH 已就绪 {1}", peers().filter(item => item.online).length + 1, readySessions().length)),
        ui.syncError ? button(t("重新读取"), 'retry', { class: 'link' }) : null));
    patchChildren(root, [content]);
  }
  function toast(text) {
    toastNode.textContent = i18n.message(text); toastNode.hidden = false; clearTimeout(ui.toastTimer);
    ui.toastTimer = setTimeout(() => { toastNode.hidden = true; }, 4000);
  }
  function applySnapshot(snapshot) {
    if (!snapshot || typeof snapshot.logged_in !== 'boolean') return false;
    if (snapshot.logged_in !== s()?.logged_in || snapshot.username !== s()?.username || snapshot.node_id !== localId()) resetHistory();
    const wasLogged = s()?.logged_in; ui.snapshot = snapshot; ui.syncError = ''; ui.loadError = '';
    if (!ui.selected || (!wasLogged && snapshot.logged_in)) {
      ui.selected = snapshot.node_id; ui.view = 'device'; ui.query = ''; ui.failureGroup = null;
    }
    if (!snapshot.logged_in && wasLogged) {
      ui.auth.password = ''; ui.auth.confirm_password = ''; ui.drafts.clear();
      ui.authError = snapshot.last_error ? message(snapshot.last_error) : '';
      closeMenu(); if (modal.open) modal.close();
    }
    if (!ui.auth.username) ui.auth.username = snapshot.last_username || snapshot.username || '';
    if (ui.selected && ui.selected !== snapshot.node_id && !device(ui.selected)) {
      ui.selected = snapshot.node_id; if (ui.view !== 'history') ui.view = 'device';
    }
    return true;
  }
  async function refresh() {
    const epoch = ui.epoch; const request = ++ui.request;
    try {
      const snapshot = await api('client_state');
      if (epoch !== ui.epoch || request < ui.applied || ui.mutating) return;
      ui.applied = request; applySnapshot(snapshot); render();
    } catch (error) { if (epoch === ui.epoch) { if (s()) ui.syncError = message(error); else ui.loadError = message(error); render(); } }
  }
  async function polling() {
    await refresh(); clearTimeout(ui.poll);
    ui.poll = setTimeout(polling, s()?.logged_in && (hasWork() || testBusy()) ? 1000 : 3000);
  }
  async function mutate(key, command, args = {}) {
    if (ui.busy.has(key)) return null;
    ui.busy.add(key); ++ui.epoch; ++ui.mutating; const mutation = ++ui.mutation; render();
    try {
      const result = await api(command, args);
      if (mutation >= ui.mutationApplied) { ui.mutationApplied = mutation; applySnapshot(result); }
      return result;
    } finally { ++ui.epoch; --ui.mutating; ui.busy.delete(key); render(); refresh(); }
  }
  function navigate(view, id, focus = true) {
    ui.view = view; if (id) ui.selected = id; closeMenu(); render();
    if (focus) document.querySelector('[data-page-heading]')?.focus({ preventScroll: true });
  }
  function closeMenu(restoreFocus = false) {
    menu.hidden = true; ui.menuNode = null;
    if (restoreFocus && ui.menuReturn?.isConnected) ui.menuReturn.focus();
    ui.menuReturn = null;
  }
  function closeLanguageMenu(restoreFocus = false) {
    ui.languageOpen = false; languageMenu.hidden = true;
    const trigger = document.getElementById('language-trigger');
    trigger?.setAttribute('aria-expanded', 'false');
    if (restoreFocus) trigger?.focus();
  }
  function renderLanguageMenu() {
    languageMenu.hidden = !ui.languageOpen;
    menu.setAttribute('aria-label', t('设备操作'));
    if (!ui.languageOpen) return;
    const choices = [['system', '跟随系统 / System'], ['zh-CN', '简体中文'], ['en', 'English']];
    patchChildren(languageMenu, [h('div', { class: 'language-menu-title', 'data-key': 'title' }, '语言 / Language'),
      ...choices.map(([value, label]) => button([h('span', { lang: value === 'system' ? undefined : value }, label),
        h('span', { class: 'language-check', 'aria-hidden': 'true' }, i18n.preference() === value ? icon('check') : null)], 'set-language',
      { class: 'language-option', role: 'menuitemradio', 'aria-checked': String(i18n.preference() === value), 'data-language': value, 'data-key': value })),
      h('div', { class: 'language-menu-foot', 'data-key': 'system' }, t('系统当前：{0}', i18n.systemLocale() === 'zh-CN' ? '简体中文' : 'English'))]);
    const rect = document.getElementById('language-trigger')?.getBoundingClientRect();
    if (rect) {
      languageMenu.style.left = `${Math.max(6, Math.min(rect.right - languageMenu.offsetWidth, innerWidth - languageMenu.offsetWidth - 6))}px`;
      languageMenu.style.top = `${Math.min(rect.bottom + 6, innerHeight - languageMenu.offsetHeight - 6)}px`;
    }
  }
  function openMenu(id, x, y) {
    closeLanguageMenu();
    const item = device(id); if (!item) return;
    const isLocal = id === localId(); const active = !!sessionFor(id) || workingStates.has(operation(id)?.state);
    menu.replaceChildren(...[
      !isLocal ? button(active ? t("终止连接") : t("连接设备"), active ? 'terminate-one' : 'configure', { role: 'menuitem', 'data-id': id, disabled: !active && !!blockReason(item) }) : null,
      button(t("重命名设备"), 'rename', { role: 'menuitem', 'data-id': id, disabled: !localRegistered() }),
      button(t("删除设备"), 'delete-device', { role: 'menuitem', class: 'danger', 'data-id': id, disabled: !localRegistered() || ui.busy.has('delete-device') }),
    ].filter(Boolean));
    ui.menuNode = id; ui.menuReturn = document.activeElement; menu.hidden = false;
    menu.style.left = `${Math.max(6, Math.min(x, innerWidth - menu.offsetWidth - 6))}px`;
    menu.style.top = `${Math.max(6, Math.min(y, innerHeight - menu.offsetHeight - 6))}px`;
    menu.querySelector('button:not(:disabled)')?.focus();
  }
  function showModal(kind, contents) {
    closeMenu(); closeLanguageMenu(); ui.modalKind = kind; ui.modalRender = contents;
    modal.replaceChildren(...contents()); modal.showModal();
  }
  function renameDialog(id) {
    const item = device(id); if (!item) return;
    showModal('rename', () => [h('h2', { id: 'modal-title' }, t("重命名设备")), h('form', { 'data-form': 'rename', 'data-id': id },
      deviceNameField(t("设备名称"), 'rename-name', item.display_name, { name: 'display_name', maxlength: 80, required: true }),
      h('p', { class: 'muted name-sync-note' }, t("名称将同步到账号下的设备列表，当前连接保持不变。")),
      h('div', { class: 'actions' }, button(t("取消"), 'close-modal'), button(t("保存"), '', { type: 'submit', class: 'button primary' })))]);
    const input = document.getElementById('rename-name'); input.focus(); input.select();
  }
  function confirmAll() {
    showModal('terminate', () => [h('h2', { id: 'modal-title' }, t("终止全部连接？")),
      h('p', { class: 'modal-copy' }, t("将结束本机发起的全部连接，并取消正在建立的连接和等待任务。现有 SSH 会中断，之后不会自动恢复。")),
      h('div', { class: 'actions' }, button(t("保留连接"), 'close-modal'), button(t("终止全部连接"), 'confirm-terminate-all', { class: 'button danger' }))]);
  }
  function deleteDialog(id) {
    const item = device(id); if (!item) return;
    const local = id === localId();
    showModal('delete-device', () => [h('h2', { id: 'modal-title' }, t("删除设备？")),
      h('p', { class: 'modal-copy' }, t("将从账号的设备列表中删除“{0}”。不会删除这台电脑上的文件。", item.display_name)),
      h('p', { class: 'modal-copy' }, local ? t("删除本机后会退出登录；重新登录并确认重新绑定后可再次添加。") : t("设备在线时也会被移除并退出登录；需要在该电脑重新登录并确认重新绑定后才能再次添加。")),
      h('p', { class: 'muted' }, t("请先终止该设备的连接、等待任务或连接测试。")),
      h('div', { class: 'actions' }, button(t("取消"), 'close-modal'), button(t("删除设备"), 'confirm-delete-device', { class: 'button danger', 'data-id': id, disabled: ui.busy.has('delete-device') }))]);
  }
  function rebindDialog() {
    if (!s()?.device_rebind_required) return;
    showModal('rebind-device', () => [h('h2', { id: 'modal-title' }, t("确认重新绑定这台电脑")),
      h('p', { class: 'modal-copy' }, t("请先终止旧设备的活动连接。此操作会替换账号下这台设备的旧凭据并吊销旧凭据，旧客户端将无法继续访问。请仅在确认这是自己的设备时继续。")),
      h('form', { 'data-form': 'rebind-device' },
        field(t("账号密码"), 'rebind-password', '', { name: 'password', type: 'password', autocomplete: 'current-password', required: true, minlength: 4 }),
        h('p', { class: 'auth-error', role: 'alert', 'data-rebind-error': '', 'data-copyable': '' }),
        h('div', { class: 'actions' }, button(t("取消"), 'close-modal'), button(t("确认替换旧凭据并重新绑定"), '', { type: 'submit', class: 'button danger' })))]);
    document.getElementById('rebind-password')?.focus();
  }
  function showWindowsHelp() {
    if (!isWindows()) return;
    showModal('windows-help', () => [h('h2', { id: 'modal-title' }, t("Windows 处理方法")), h('div', { class: 'modal-copy' },
      h('p', {}, t("遇到防火墙提示时，先核对 StarBridge 及其连接组件的名称和路径，再允许必要的网络通信。")),
      h('p', {}, t("如文件被隔离，可在「Windows 安全中心 → 病毒和威胁防护 → 保护历史记录」查看记录。先核对程序来源和官方校验值，确认属于误报后再按提示放行。")),
      h('p', {}, t("如果电脑由组织管理，请联系管理员。不要关闭防护或添加宽泛的文件夹排除项。"))),
      h('div', { class: 'actions' }, button(t("知道了"), 'close-modal', { class: 'button primary' }))]);
  }
  async function writeClipboard(text) {
    if (desktop) await desktop.invoke('plugin:clipboard-manager|write_text', { text });
    else await navigator.clipboard.writeText(text);
    toast(t("已复制到剪贴板"));
  }
  async function fetchExport(copy = false) {
    if (ui.busy.has('export')) return;
    const requestedFingerprint = fingerprint(); const epoch = ui.epoch;
    const username = s()?.username; const nodeId = localId();
    if (!requestedFingerprint || !s()?.logged_in || ui.syncError) {
      ui.exportData = null; ui.exportFingerprint = null; render();
      if (copy) throw new Error(t("当前没有可复制的有效 SSH 连接。"));
      return;
    }
    ui.busy.add('export'); render();
    try {
      // The backend revalidates readiness at export time, rather than trusting a stale UI snapshot.
      const result = await api('client_export', { form: { target_node_ids: ui.exportIds } });
      if (epoch !== ui.epoch || !s()?.logged_in || username !== s()?.username || nodeId !== localId()
        || ui.syncError || requestedFingerprint !== fingerprint() || !result?.items?.length || !result?.text) {
        ui.exportData = null; ui.exportFingerprint = null;
        if (copy) throw new Error(t("连接状态已变化，请重新获取连接信息。"));
        return;
      }
      ui.exportData = result; ui.exportFingerprint = requestedFingerprint;
      if (copy) {
        await writeClipboard(exportText(result));
      }
    } finally { ui.busy.delete('export'); render(); }
  }
  async function action(target, event) {
    const actionName = target.dataset.action; const id = target.dataset.id;
    if (target.disabled) return;
    if (actionName !== 'menu') closeMenu();
    switch (actionName) {
      case 'language-menu':
        ui.languageOpen = !ui.languageOpen; render();
        if (ui.languageOpen) languageMenu.querySelector('[aria-checked="true"]')?.focus();
        break;
      case 'set-language': {
        closeLanguageMenu();
        const persisted = i18n.setPreference(target.dataset.language);
        if (!persisted) toast(t('语言偏好未能保存，本次切换仍然有效。'));
        document.getElementById('language-trigger')?.focus();
        break;
      }
      case 'window-minimize': case 'window-close':
        if (frameless) await desktop.invoke('client_window_action', { action: actionName === 'window-close' ? 'close' : 'minimize' });
        break;
      case 'retry': await refresh(); break;
      case 'history': navigate('history'); await loadHistory(); break;
      case 'history-refresh': await loadHistory(); break;
      case 'history-more': await loadHistory(true); break;
      case 'history-direction':
        if (['all', 'outgoing', 'incoming'].includes(target.dataset.direction)) { ui.historyDirection = target.dataset.direction; ui.historyExpanded = null; render(); }
        break;
      case 'history-sort': ui.historyAscending = !ui.historyAscending; render(); break;
      case 'history-details': ui.historyExpanded = ui.historyExpanded === target.dataset.session ? null : target.dataset.session; render(); break;
      case 'rebind-device': rebindDialog(); break;
      case 'select-device': ui.failureGroup = null; navigate('device', id, false); break;
      case 'device': navigate('device'); break;
      case 'configure': navigate('configure', id || ui.selected); break;
      case 'all': ui.allConfig = !s()?.batch; ui.failureGroup = null; navigate('all'); break;
      case 'all-config': ui.allConfig = true; navigate('all'); break;
      case 'all-progress': ui.allConfig = false; navigate('all'); break;
      case 'start-one': {
        const item = device(id || ui.selected); const reason = blockReason(item); if (reason) throw new Error(reason);
        await mutate('connect', 'client_connect', { form: { target_node_id: item.node_id, policy: ui.policy } }); navigate('device', item.node_id); break;
      }
      case 'start-all': {
        if (s()?.batch?.running || ui.busy.has('connect-all')) return;
        await mutate('connect-all', 'client_connect_all', { form: { policy: ui.policy, rebuild_relay: ui.rebuild } });
        ui.allConfig = false; navigate('all'); break;
      }
      case 'terminate-one':
        await mutate('terminate', 'client_terminate', { form: { target_node_id: id } }); toast(t("该设备的连接已终止")); break;
      case 'terminate-all': confirmAll(); break;
      case 'confirm-terminate-all': modal.close(); await mutate('terminate', 'client_terminate', { form: { target_node_id: null } }); toast(t("全部连接及等待任务已终止")); break;
      case 'menu': { const rect = target.getBoundingClientRect(); openMenu(id, rect.right - 155, rect.bottom + 4); break; }
      case 'rename': renameDialog(id); break;
      case 'delete-device': deleteDialog(id); break;
      case 'confirm-delete-device': {
        if (ui.busy.has('delete-device')) return;
        const result = await mutate('delete-device', 'client_delete_device', { form: { node_id: id } });
        ui.drafts.delete(id); ui.exportData = null; ui.exportFingerprint = null;
        if (modal.open) modal.close();
        if (result.logged_in) navigate('device', result.node_id);
        toast(t("设备已删除")); break;
      }
      case 'close-modal': modal.close(); break;
      case 'auth-switch': ui.authMode = ui.authMode === 'login' ? 'register' : 'login'; ui.authError = ''; ui.auth.password = ''; ui.auth.confirm_password = ''; render(); break;
      case 'logout': {
        if (hasWork() || testBusy()) {
          showModal('logout', () => [h('h2', { id: 'modal-title' }, t("退出登录？")), h('p', { class: 'modal-copy' }, t("退出将结束本机连接及正在进行的任务，并清除可恢复的登录凭据。")),
            h('div', { class: 'actions' }, button(t("取消"), 'close-modal'), button(t("退出登录"), 'confirm-logout', { class: 'button danger' }))]);
        } else await mutate('logout', 'client_logout');
        break;
      }
      case 'confirm-logout': modal.close(); await mutate('logout', 'client_logout'); break;
      case 'copy-ssh': {
        const result = await api('client_export', { form: { target_node_ids: [id] } });
        const item = result?.items?.[0];
        const command = item?.ssh_command || item?.command;
        if (!command) throw new Error(t("该连接尚未就绪或已经结束，请重新连接后再复制。"));
        await writeClipboard(command); break;
      }
      case 'export-one': case 'export-all':
        ui.exportIds = actionName === 'export-one' ? [id] : []; ui.exportBack = ui.view; ui.exportData = null; ui.exportFingerprint = fingerprint(); navigate('export'); await fetchExport(); break;
      case 'refresh-export': await fetchExport(); break;
      case 'copy-export': await fetchExport(true); break;
      case 'export-back': navigate(ui.exportBack); break;
      case 'test': navigate('test', localId()); break;
      case 'start-test': if (testGate()) throw new Error(testGate()); await mutate('start-test', 'client_start_test'); break;
      case 'cancel-test': await mutate('cancel-test', 'client_cancel_test'); break;
      case 'test-explanation': showModal('test-explanation', () => [h('h2', { id: 'modal-title' }, t("连接测试说明")), testExplanation(), h('div', { class: 'actions' }, button(t("知道了"), 'close-modal'))]); break;
      case 'components': if (modal.open) modal.close(); navigate('components'); ui.components = await api('client_components'); render(); break;
      case 'windows-help': if (modal.open) modal.close(); showWindowsHelp(); break;
      case 'copy-url': await writeClipboard(target.dataset.url); break;
      case 'failure-category': {
        const ids = failures().get(target.dataset.category); if (ids?.length) { ui.failureGroup = ids; navigate('device', ids[0]); } break;
      }
      case 'failure-prev': case 'failure-next': {
        const ids = ui.failureGroup || []; if (!ids.length) return;
        const index = ids.indexOf(ui.selected); navigate('device', ids[(index + (actionName === 'failure-next' ? 1 : -1) + ids.length) % ids.length]); break;
      }
      default: break;
    }
  }
  function guardClipboard(event) {
    const target = event.target instanceof Element ? event.target : event.target?.parentElement;
    // Input selections are separate from document selections; never use a stale document range for them.
    if (target?.matches('input, textarea')) {
      if (target.matches(':read-write') || (event.type === 'copy' && target.matches('[data-copyable]:not(:disabled)'))) return;
      event.preventDefault(); return;
    }
    const selection = window.getSelection();
    if (selection?.rangeCount === 1 && !selection.isCollapsed) {
      const range = selection.getRangeAt(0);
      const ancestor = range.commonAncestorContainer;
      const element = ancestor instanceof Element ? ancestor : ancestor.parentElement;
      // A range crossing a copyable value and surrounding UI has no permitted common ancestor.
      if (element?.isContentEditable && !range.cloneContents().querySelector('[contenteditable="false"]')) return;
      if (event.type === 'copy' && element?.closest('[data-copyable]')) return;
    }
    event.preventDefault();
  }
  document.addEventListener('copy', guardClipboard);
  document.addEventListener('cut', guardClipboard);
  document.addEventListener('mousedown', event => {
    if (!frameless || event.button !== 0 || event.detail > 1 || !event.target.closest('.app-header')
      || event.target.closest('button, a, input, textarea, select, [contenteditable], [role="button"]')) return;
    event.preventDefault();
    desktop.invoke('client_window_action', { action: 'drag' }).catch(error => toast(message(error)));
  });
  document.addEventListener('dblclick', event => {
    if (frameless && event.target.closest('.app-header') && !event.target.closest('button, a, input, textarea, select')) event.preventDefault();
  });
  document.addEventListener('click', event => {
    if (!event.composedPath().some(node => node === languageMenu || node.id === 'language-trigger')) closeLanguageMenu();
    const target = event.target.closest('[data-action]');
    if (target) action(target, event).catch(error => toast(message(error)));
    else if (!menu.contains(event.target)) closeMenu();
  });
  document.addEventListener('contextmenu', event => {
    const row = event.target.closest('.device-row'); if (!row) return;
    event.preventDefault(); openMenu(row.dataset.id, event.clientX, event.clientY);
  });
  document.addEventListener('input', event => {
    const input = event.target;
    if (input.id === 'device-search') { ui.query = input.value; render(); }
    else if (input.closest('[data-form="auth"]') && input.name !== 'remember') { ui.auth[input.name] = input.value; ui.authError = ''; }
    else if (input.dataset.draft) {
      const item = device(input.dataset.draft); if (!item) return;
      const draft = { ...draftFor(item), [input.name]: input.value }; ui.drafts.set(item.node_id, draft); render();
    }
  });
  document.addEventListener('change', event => {
    const input = event.target;
    if (input.id === 'auth-remember') ui.auth.remember = input.checked;
    if (input.id === 'history-failed') { ui.historyFailed = input.checked; ui.historyExpanded = null; render(); }
    if (input.name === 'connection-policy') { ui.policy = input.value; render(); }
    if (input.name === 'relay-choice') { ui.rebuild = input.value === 'rebuild'; render(); }
  });
  document.addEventListener('submit', event => {
    const form = event.target; if (!form.dataset.form) return; event.preventDefault();
    (async () => {
      if (form.dataset.form === 'auth') {
        if (ui.busy.has('auth')) return;
        ui.auth.username = ui.auth.username.trim();
        if (!ui.auth.username) { ui.authError = t("请输入用户名。"); render(); return; }
        if (ui.authMode === 'register' && ui.auth.password !== ui.auth.confirm_password) { ui.authError = t("两次输入的密码不一致。"); render(); document.getElementById('auth-confirm')?.focus(); return; }
        ui.authError = '';
        try { await mutate('auth', ui.authMode === 'register' ? 'client_register' : 'client_login', { form: { ...ui.auth } }); ui.auth.password = ''; ui.auth.confirm_password = ''; ui.view = 'device'; ui.selected = localId(); render(); }
        catch (error) { ui.authError = message(error); render(); }
      } else if (form.dataset.form === 'rebind-device') {
        if (ui.busy.has('rebind-device') || !s()?.device_rebind_required) return;
        const password = String(new FormData(form).get('password') || '');
        form.reset();
        const submit = form.querySelector('button[type="submit"]'); submit.disabled = true;
        try {
          await mutate('rebind-device', 'client_rebind_device', { form: { password } });
          modal.close(); toast(t("设备已重新绑定，旧凭据已吊销"));
        } catch (error) {
          const alert = form.querySelector('[data-rebind-error]'); if (alert) { alert.dataset.errorSource = message(error); alert.textContent = displayMessage(error); }
          document.getElementById('rebind-password')?.focus();
        } finally { if (submit.isConnected) submit.disabled = false; }
      } else if (form.dataset.form === 'metadata' || form.dataset.form === 'rename') {
        const item = device(form.dataset.id); if (!item || ui.busy.has('metadata')) return;
        const rename = form.dataset.form === 'rename'; const values = new FormData(form);
        const draft = rename ? { display_name: String(values.get('display_name') || '').trim(), purpose: item.purpose || '' } : draftFor(item);
        if (!draft.display_name.trim()) throw new Error(t("设备名称不能为空。"));
        const submitted = JSON.stringify(draft);
        await mutate('metadata', 'client_update_device', { form: { node_id: item.node_id, display_name: draft.display_name.trim(), purpose: draft.purpose } });
        if (rename && ui.drafts.has(item.node_id)) {
          ui.drafts.set(item.node_id, { ...ui.drafts.get(item.node_id), display_name: draft.display_name.trim() });
        } else if (JSON.stringify(ui.drafts.get(item.node_id)) === submitted) ui.drafts.delete(item.node_id);
        if (rename) modal.close(); render(); toast(t("设备信息已保存"));
      }
    })().catch(error => toast(message(error)));
  });
  modal.addEventListener('close', () => {
    for (const input of modal.querySelectorAll('input[type="password"]')) input.value = '';
  });
  document.addEventListener('keydown', event => {
    if (!ui.languageOpen && event.target.id === 'language-trigger' && ['ArrowDown', 'ArrowUp'].includes(event.key)) {
      event.preventDefault(); closeMenu(); ui.languageOpen = true; render();
      const choices = languageMenu.querySelectorAll('button');
      choices[event.key === 'ArrowUp' ? choices.length - 1 : 0]?.focus(); return;
    }
    if (ui.languageOpen) {
      if (event.key === 'Escape' || event.key === 'Tab') { closeLanguageMenu(true); if (event.key === 'Escape') event.preventDefault(); return; }
      if (['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
        event.preventDefault();
        const choices = [...languageMenu.querySelectorAll('button')]; const current = choices.indexOf(document.activeElement);
        const index = event.key === 'Home' ? 0 : event.key === 'End' ? choices.length - 1 : (current + (event.key === 'ArrowDown' ? 1 : -1) + choices.length) % choices.length;
        choices[index]?.focus(); return;
      }
    }
    if (event.key === 'Escape') { if (!menu.hidden) { closeMenu(true); event.preventDefault(); } return; }
    if (!menu.hidden && ['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
      event.preventDefault(); const buttons = [...menu.querySelectorAll('button:not(:disabled)')]; const current = buttons.indexOf(document.activeElement);
      const index = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (current + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length;
      buttons[index]?.focus(); return;
    }
    const row = event.target.closest('.device-row');
    if (row && ((event.shiftKey && event.key === 'F10') || event.key === 'ContextMenu')) { event.preventDefault(); const rect = row.getBoundingClientRect(); openMenu(row.dataset.id, rect.left + 30, rect.bottom); }
    if (row && ['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
      event.preventDefault(); const rows = [...document.querySelectorAll('.device-row')]; const index = rows.indexOf(row);
      rows[event.key === 'Home' ? 0 : event.key === 'End' ? rows.length - 1 : Math.max(0, Math.min(rows.length - 1, index + (event.key === 'ArrowDown' ? 1 : -1)))]?.focus();
    }
  });
  document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });
  modal.addEventListener('close', () => { ui.modalKind = null; ui.modalRender = null; });
  window.addEventListener('resize', () => { if (ui.languageOpen) renderLanguageMenu(); });
  window.addEventListener('starbridge:language', () => {
    syncDesktopLocale();
    closeMenu(); toastNode.hidden = true; render();
    if (modal.open && ui.modalRender) {
      const values = [...modal.querySelectorAll('input, textarea')].map(input => ({ id: input.id, name: input.name, value: input.value, checked: input.checked }));
      const error = modal.querySelector('[data-rebind-error]')?.dataset.errorSource;
      patchChildren(modal, ui.modalRender());
      for (const saved of values) {
        const input = [...modal.querySelectorAll('input, textarea')].find(item => saved.id ? item.id === saved.id : item.name === saved.name);
        if (input) { input.value = saved.value; input.checked = saved.checked; }
      }
      if (error) { const alert = modal.querySelector('[data-rebind-error]'); if (alert) { alert.dataset.errorSource = error; alert.textContent = displayMessage(error); } }
    }
  });
  async function init() {
    await syncDesktopLocale();
    try {
      const [snapshot, config] = await Promise.all([api('client_state'), api('client_config')]);
      ui.config = config; ui.policy = ['direct_preferred', 'direct_only'].includes(config.connection_policy) ? config.connection_policy : config.direct_only ? 'direct_only' : 'direct_preferred';
      ui.rebuild = config.rebuild_relay === true; applySnapshot(snapshot);
      ui.auth.username = snapshot.last_username || snapshot.username || '';
      ui.auth.remember = snapshot.remember_login !== false;
    } catch (error) { ui.loadError = message(error); }
    render(); ui.poll = setTimeout(polling, 1200);
  }
  render();
  init();
})();
