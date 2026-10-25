'use strict';

(() => {
  const root = document.getElementById('app');
  const menu = document.getElementById('context-menu');
  const modal = document.getElementById('modal');
  const toastNode = document.getElementById('toast');
  const desktop = window.__TAURI__?.core;
  const frameless = !!desktop && window.__STARBRIDGE_FRAMELESS__ === true;
  const activeStates = new Set(['negotiating', 'connected']);
  const workingStates = new Set(['queued', 'connecting', 'verifying']);
  const ui = {
    snapshot: null, config: null, view: 'device', selected: null, query: '',
    authMode: 'login', auth: { username: '', password: '', confirm_password: '', remember: true },
    authError: '', loadError: '', syncError: '', busy: new Set(), drafts: new Map(),
    policy: 'direct_preferred', rebuild: false, allConfig: true, failureGroup: null,
    components: null, exportData: null, exportIds: [], exportFingerprint: '', exportBack: 'device',
    epoch: 0, poll: null, request: 0, applied: 0, mutating: 0, mutation: 0, mutationApplied: 0, menuNode: null, menuReturn: null, modalKind: null,
  };
  const routes = {
    client_state: ['GET', '/api/v1/state'], client_config: ['GET', '/api/v1/config'],
    client_save_config: ['PUT', '/api/v1/config'], client_login: ['POST', '/api/v1/login'],
    client_register: ['POST', '/api/v1/register'], client_logout: ['POST', '/api/v1/logout'],
    client_update_device: ['PUT', '/api/v1/devices'], client_connect: ['POST', '/api/v1/connect'],
    client_connect_all: ['POST', '/api/v1/connect/all'], client_terminate: ['POST', '/api/v1/terminate'],
    client_export: ['POST', '/api/v1/export'], client_start_test: ['POST', '/api/v1/test/start'],
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
    try { result = body ? JSON.parse(body) : null; } catch { throw new Error('客户端返回了无法读取的响应，请稍后重试。'); }
    if (!response.ok) throw new Error(result?.error || result?.message || `请求未完成（${response.status}）`);
    return result;
  }
  function message(value) {
    const raw = value instanceof Error ? value.message : typeof value === 'string' ? value : value?.message || '操作未完成，请重试。';
    return raw.replace(/https?:\/\/[^\s)）]+/g, '服务地址');
  }
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
      return { ...(s()?.devices || []).find(item => item.node_id === id), node_id: id, display_name: s()?.display_name || '这台电脑',
        purpose: s()?.purpose || '', os: s()?.platform || (s()?.devices || []).find(item => item.node_id === id)?.os || '', online: true };
    }
    return peers().find(item => item.node_id === id);
  }
  const name = id => device(id)?.display_name || '其他设备';
  const sessionFor = id => sessions().find(item => item.target_node_id === id && item.initiator_node_id === localId()) || sessions().find(item => item.initiator_node_id === id && item.target_node_id === localId());
  const ready = id => {
    const session = sessionFor(id);
    return session?.initiator_node_id === localId() && session.status === 'connected' && session.ssh_ready === true && !!session.ssh_command;
  };
  const readySessions = () => outgoing().filter(session => ready(session.target_node_id));
  const hasWork = () => sessions().length > 0 || ops().some(item => workingStates.has(item.state)) || s()?.batch?.running;
  const hasOutgoingWork = () => outgoing().length > 0 || ops().some(item => workingStates.has(item.state)) || s()?.batch?.running;
  const pathLabel = path => ({ direct: '直连', relay: '中转', unknown: '正在协商' })[path] || '正在协商';
  function stageLabel(stage) {
    return ({ queued: '等待连接', preparing: '准备连接组件', components: '准备连接组件', contact: '联系设备', contacting: '联系设备', negotiating: '正在协商连接',
      direct: '正在尝试直连', relay: '正在建立中转连接', verifying: '验证 SSH 登录', ssh_verifying: '验证 SSH 登录',
      ssh: '验证 SSH 登录', ready: 'SSH 已就绪', failed: '连接失败', terminated: '已终止连接' })[stage] || stage || '正在连接';
  }
  function blockReason(item) {
    if (!item || item.node_id === localId()) return '这是本机。';
    if (testBusy()) return '连接测试进行中，请等待测试结束。';
    if (incoming().length || device(localId())?.occupancy === 'incoming') return '本机正被其他设备连接，请先终止连接。';
    if (sessionFor(item.node_id)) return '';
    if (!item.online) return '设备离线，暂时无法连接。';
    if (isEnding(item)) return '正在结束连接，确认结束后可重新连接。';
    if (item.occupancy && item.occupancy !== 'idle') return '已被其他设备连接';
    if (Array.isArray(item.capabilities) && !item.capabilities.includes('ssh_server')) return '此设备暂不支持接收连接。';
    return '';
  }
  function stateFor(item) {
    if (item.node_id === localId()) {
      if (testBusy()) return { text: test().state === 'cleaning' ? '正在结束测试' : '连接测试中', tone: 'warn' };
      const inbound = incoming()[0];
      if (inbound) return inbound.status === 'connected'
        ? { text: `已被${name(inbound.initiator_node_id)}连接`, tone: 'good' }
        : { text: `${name(inbound.initiator_node_id)}正在连接本机`, tone: 'warn' };
      if (readySessions().length) return { text: `已控制${readySessions().length}台设备`, tone: 'good' };
      const pending = ops().filter(item => workingStates.has(item.state)).length;
      if (outgoing().length || pending) return { text: `正在连接${Math.max(outgoing().length, pending)}台设备`, tone: 'warn' };
      if (item.occupancy === 'incoming') return { text: `正在接收${name(item.occupied_by)}的连接`, tone: 'warn' };
      return { text: '在线', tone: 'good' };
    }
    const session = sessionFor(item.node_id); const op = operation(item.node_id);
    if (session?.target_node_id === localId()) return session.status === 'connected'
      ? { text: '已连接本机', tone: 'good' }
      : { text: '正在连接本机', tone: 'warn' };
    if (ready(item.node_id)) return { text: `${pathLabel(session.path)} · 已连接`, tone: 'good' };
    if (op && workingStates.has(op.state)) return { text: stageLabel(op.stage), tone: 'warn' };
    if (session) return { text: session.status === 'connected' ? '正在验证 SSH' : '正在连接', tone: 'warn' };
    if (!item.online) return { text: '离线', tone: 'muted' };
    if (isEnding(item)) return { text: '正在结束连接', tone: 'warn' };
    if (item.occupancy && item.occupancy !== 'idle') return { text: '已被其他设备连接', tone: 'muted' };
    if (op?.state === 'terminated') return { text: '连接已终止', tone: 'muted' };
    if (op?.state === 'failed') return { text: '连接失败', tone: 'bad' };
    return { text: '在线', tone: 'good' };
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
    return ({ idle: '未进行连接测试', running: '连接测试中', cleaning: '正在结束测试', passed: '连接测试成功',
      failed: '连接测试失败', cleanup_failed: '连接测试失败 · 清理未完成', cancelled: '未完成连接测试', unverified: '尚未验证' })[testState()];
  }
  const testTone = () => testState() === 'passed' ? 'good' : ['failed', 'cleanup_failed'].includes(testState()) ? 'bad' : testBusy() ? 'warn' : 'muted';
  function testGate() {
    if (testBusy()) return '';
    if (hasWork()) return '请先终止现有连接及连接任务，再开始测试。';
    if (s()?.allow_relay === false || s()?.direct_only === true) return '当前策略不允许中转，本机连接测试尚未验证。';
    if (device(localId())?.occupancy && device(localId()).occupancy !== 'idle') return '本机正在参与连接，请结束连接后再测试。';
    return '';
  }
  const isWindows = () => /windows|win32/i.test(String(s()?.platform || device(localId())?.os || ''));
  function field(label, id, value, attrs = {}) {
    return h('label', { class: 'field', 'data-key': `field-${id}`, for: id }, label, h(attrs.multiline ? 'textarea' : 'input', { id, value, ...attrs, multiline: undefined }));
  }
  function metadata(item) {
    return h('dl', { class: 'meta', 'data-key': 'metadata' },
      h('dt', {}, '系统'), h('dd', {}, item.os || '未提供'),
      h('dt', {}, '设备 ID'), h('dd', {}, item.node_id),
      h('dt', {}, '登录用户'), h('dd', {}, item.ssh_user || (item.node_id === localId() ? s()?.ssh?.login_user : '') || '待连接后确认'));
  }
  function heading(title, subtitle, item) {
    return h('div', { class: 'detail-head', 'data-key': 'heading' }, h('span', { class: 'glyph' }, icon('monitor')),
      h('div', { class: 'heading-copy' }, h('h1', { tabindex: '-1', 'data-page-heading': '' }, title), h('p', { class: 'muted' }, subtitle)),
      item ? button('', 'menu', { class: 'icon-button', 'aria-label': `更多设备操作：${item.display_name}`, 'aria-haspopup': 'menu', 'data-id': item.node_id }, 'more') : null);
  }
  function banner(title, detail, tone = '') {
    return h('div', { class: `banner ${tone}`, 'data-key': 'status-banner' }, h('p', { class: 'banner-title', title }, title), detail ? h('p', { class: 'muted' }, detail) : null);
  }
  function connectionError(error, stage) {
    const detail = message(error || '连接未完成，请重试。');
    if (/REMOTE HOST IDENTIFICATION|Host key verification failed|host.?key.*(?:mismatch|changed)|主机身份.*(?:不符|失败)/i.test(detail)) {
      return { summary: '设备身份校验未通过，连接已停止，请核实设备身份后重试。', detail };
    }
    if (detail.length <= 160 && detail.split('\n').length <= 3) return { summary: detail, detail: '' };
    const phase = ({ components: '连接组件启动失败', contact: '设备联系失败', direct: '直连未成功', relay: '中转连接失败', ssh: 'SSH 验证失败' })[stage] || '连接失败';
    let cause;
    if (/permission denied|authentication failed|认证失败|登录失败/i.test(detail)) cause = '登录权限或凭据未通过验证';
    else if (/timed? ?out|timeout|超时/i.test(detail)) cause = '连接超时';
    else if (/connection refused|连接被拒绝|拒绝连接/i.test(detail)) cause = '目标拒绝了连接';
    else if (/no such file|not found|找不到|文件缺失|组件缺失/i.test(detail)) cause = '所需文件或连接组件缺失';
    else if (/exited|启动后退出|启动失败/i.test(detail)) cause = '连接组件未能持续运行';
    else cause = detail.split('\n').find(line => line.trim() && !/^[@\s]+$/.test(line))?.trim().slice(0, 90) || '未能完成连接';
    return { summary: `${phase}：${cause}。连接已停止，具体原因见技术详情。`, detail };
  }
  function row(item, local = false) {
    const status = stateFor(item); const active = ui.selected === item.node_id && ['device', 'configure'].includes(ui.view);
    return h('button', { type: 'button', class: `device-row${active ? ' active' : ''}`, 'data-key': item.node_id,
      'data-action': 'select-device', 'data-id': item.node_id, 'aria-pressed': active, 'aria-label': `${item.display_name}${local ? '，本机' : ''}，${status.text}` },
    h('span', { class: 'glyph' }, icon('monitor')), h('span', { class: 'identity' }, h('span', { class: 'device-name', title: item.display_name }, item.display_name),
      h('span', { class: 'device-sub', title: item.purpose || item.os || '' }, `${item.purpose || item.os || '未填写用途'}${local ? '（本机）' : ''}`)),
    h('span', { class: `row-status ${status.tone}` }, status.text));
  }
  function sidebar() {
    const all = peers(); const query = ui.query.trim().toLocaleLowerCase();
    const shown = all.filter(item => `${item.display_name || ''} ${item.purpose || ''}`.toLocaleLowerCase().includes(query))
      .sort((a, b) => rank(a) - rank(b) || (a.display_name || '').localeCompare(b.display_name || '', 'zh-CN') || a.node_id.localeCompare(b.node_id));
    return h('aside', { class: 'list-pane', 'aria-label': '我的设备', 'data-key': 'list-pane' },
      h('div', { class: 'list-head' }, h('div', { class: 'list-title' }, h('h2', {}, '我的设备'), h('span', { class: 'muted' }, `在线 ${all.filter(item => item.online).length + 1} · 离线 ${all.filter(item => !item.online).length}`)),
        h('label', { class: 'search' }, icon('search'), h('input', { type: 'search', id: 'device-search', value: ui.query, placeholder: '搜索名称或用途', 'aria-label': '搜索设备名称或用途', autocomplete: 'off' }))),
      h('div', { class: 'local-block' }, row(device(localId()), true), button(`连接测试 · ${testLabel()}`, 'test', { class: `local-test ${testTone()}`, 'aria-label': `连接测试，${testLabel()}` })),
      h('div', { class: 'device-list', 'data-key': 'device-list', 'aria-label': '其他设备' }, shown.length ? shown.map(item => row(item)) : h('div', { class: 'empty' }, query ? '没有匹配的设备' : '登录其他电脑后，设备会显示在这里。')),
      h('div', { class: 'nav-all-area' }, button([
        h('span', { class: 'nav-all-icon' }, icon('grid')),
        h('span', { class: 'identity' }, h('span', { class: 'nav-all-title' }, '连接所有设备'), h('span', { class: 'muted' }, '配置连接方式，查看进度与结果')),
        icon('arrow'),
      ], 'all', { class: `nav-all${ui.view === 'all' ? ' active' : ''}`, 'aria-current': ui.view === 'all' ? 'page' : undefined })));
  }
  function draftFor(item) {
    return ui.drafts.get(item.node_id) || { display_name: item.display_name || '', purpose: item.purpose || '' };
  }
  function editForm(item, local) {
    const draft = draftFor(item);
    return h('form', { 'data-form': 'metadata', 'data-id': item.node_id, 'data-key': `edit-${item.node_id}` },
      local ? field('本机名称', 'device-display-name', draft.display_name, { name: 'display_name', maxlength: 80, required: true, 'data-draft': item.node_id }) : null,
      field('用途备注', 'device-purpose', draft.purpose, { name: 'purpose', maxlength: 500, placeholder: '例如：开发环境、文件整理', 'data-draft': item.node_id, multiline: true }),
      button(ui.busy.has('metadata') ? '正在保存…' : '保存修改', '', { type: 'submit', disabled: ui.busy.has('metadata') || !ui.drafts.has(item.node_id) }));
  }
  function devicePage() {
    const item = device(ui.selected); if (!item) return h('div', { class: 'empty' }, '这台设备已不在账号列表中。');
    const local = item.node_id === localId(); const status = stateFor(item); const session = sessionFor(item.node_id); const op = operation(item.node_id);
    const working = workingStates.has(op?.state); const canEnd = !!session || working;
    const failure = op?.state === 'failed' ? connectionError(op.message, op.stage) : null;
    const stateDetail = local ? incoming().length ? '本机当前正在接收连接，结束后可以发起新的连接。' : '连接信息仅在当前电脑和当前会话有效。'
      : ready(item.node_id) ? `已连接${item.display_name}。SSH 登录已验证，可复制命令。`
      : failure ? failure.summary
      : blockReason(item) || (session ? message(op?.message || '网络通道建立后，还需要验证 SSH 登录。') : '点击连接后可选择连接方式。');
    return h('div', { 'data-key': `device-page-${item.node_id}` },
      ui.failureGroup ? h('div', { class: 'backline' }, button('返回连接结果', 'all', { class: 'link' }, 'back'),
        h('div', { class: 'actions' }, button('上一台', 'failure-prev', { class: 'link' }), button('下一台同类设备', 'failure-next', { class: 'link' }))) : null,
      heading(local ? '本机信息' : item.display_name, local ? '这台电脑' : item.purpose || '设备详情', item),
      banner(status.text, stateDetail, status.tone === 'good' ? 'good' : status.tone === 'bad' ? 'bad' : ''),
      failure?.detail ? h('details', { class: 'details', 'data-key': 'connection-error-details' }, h('summary', {}, '技术详情'),
        h('pre', { class: 'command' }, failure.detail)) : null,
      local ? button(h('span', { class: 'identity' }, h('strong', {}, '连接测试'), h('p', { class: testTone() }, testLabel())), 'test', {
        class: 'test-entry', 'data-key': 'local-test-entry', title: test().finished_at ? `最近一次测试：${formatTime(test().finished_at)}` : '检查本机能否正常发起和接收连接',
      }, 'test') : null,
      local ? h('div', { class: 'local-actions actions', 'data-key': 'local-actions' },
        incoming().length ? button('终止连接', 'terminate-one', { class: 'button danger', 'data-id': incoming()[0].initiator_node_id, disabled: ui.busy.has('terminate') }) : null,
        outgoing().length ? button('管理本机连接', 'all', { class: 'link' }) : null) :
      h('div', { class: 'actions stretch', 'data-key': 'connection-actions' },
        canEnd ? button('终止连接', 'terminate-one', { class: 'button danger', 'data-id': item.node_id, disabled: ui.busy.has('terminate') }) : button('连接设备', 'configure', { class: 'button primary', 'data-id': item.node_id, disabled: !!blockReason(item) || ui.busy.has('connect') }),
        ready(item.node_id) ? button('复制 SSH', 'copy-ssh', { 'data-id': item.node_id, disabled: !!ui.syncError }, 'copy') : null,
        ready(item.node_id) ? button('复制给 AI', 'export-one', { 'data-id': item.node_id, disabled: !!ui.syncError }) : null),
      ready(item.node_id) && !ui.syncError ? h('pre', { class: 'command', 'data-key': 'ssh-command', 'aria-label': '当前 SSH 命令' }, session.ssh_command) : null,
      h('div', { class: 'section', 'data-key': 'device-fields' }, editForm(item, local), metadata(item)));
  }
  function modeOptions(disabled = false) {
    const choices = [
      ['direct_preferred', '自动连接', '兼容性优先', '优先尝试设备直连；无法直连时，连接数据通过中转服务传输。信息敏感环境，请选择「仅限直连」。'],
      ['direct_only', '仅限直连', '点对点传输', '连接数据在设备间直接传输，不经中转服务。部分网络环境可能无法连接；失败后不会切换中转。'],
    ];
    return h('div', { class: 'mode-options', role: 'radiogroup', 'aria-label': '连接方式', 'data-key': 'mode-options' }, choices.map(([value, label, subtitle, copy]) =>
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
    return h('div', { class: 'bottom', 'data-key': 'existing-management' }, h('div', { class: 'section-title' }, '当前本机连接'),
      h('div', { class: 'actions stretch' }, button('终止全部连接', 'terminate-all', { class: 'button danger', disabled: ui.busy.has('terminate') }),
        button(`复制全部给 AI${readySessions().length ? `（${readySessions().length}）` : ''}`, 'export-all', { disabled: !readySessions().length || !!ui.syncError })),
      h('p', { class: 'muted' }, '终止包括已有连接、正在建立的连接和等待任务；复制仅包含已验证 SSH 的连接。'));
  }
  function allPage() {
    const batch = s()?.batch;
    if (batch && !ui.allConfig) return batchPage(batch);
    const counts = plan(); const locked = testBusy() || incoming().length > 0 || device(localId())?.occupancy === 'incoming';
    return h('div', { 'data-key': 'all-config' }, heading('连接所有设备', `账号下除本机外的全部 ${peers().length} 台设备，搜索不会改变连接范围。`),
      modeOptions(!!batch?.running),
      h('p', { class: 'muted' }, '两种模式都需要登录和连接协商。仅限直连限制连接数据路径。'),
      ui.policy === 'direct_only' && counts.relay > 0 ? h('div', { class: 'section', 'data-key': 'relay-choice' }, h('h3', {}, `已有 ${counts.relay} 台使用中转连接`),
        h('label', { class: 'choice' }, h('input', { type: 'radio', name: 'relay-choice', value: 'keep', checked: !ui.rebuild }), '保留现有中转，不计入本次直连成功'),
        h('label', { class: 'choice' }, h('input', { type: 'radio', name: 'relay-choice', value: 'rebuild', checked: ui.rebuild }), '断开这些连接，并重新尝试直连')) : null,
      h('div', { class: 'config-summary' }, summaryLine('新建连接', `${counts.fresh + counts.rebuild} 台`), summaryLine('复用当前连接', `${counts.reuse} 台`),
        counts.relay && !ui.rebuild ? summaryLine('保留现有中转', `${counts.relay} 台（不计本次直连成功）`) : null,
        summaryLine('跳过设备', `${counts.skip} 台`), counts.skip ? h('p', { class: 'muted' }, `离线 ${counts.offline} · 已被其他设备连接 ${counts.occupied}${counts.ending ? ` · 正在结束连接 ${counts.ending}` : ''} · 其他不可连接 ${counts.unsupported}`) : null),
      h('div', { class: 'bottom' }, ui.policy === 'direct_only' && ui.rebuild && counts.relay ? banner('现有 SSH 会中断', '点击开始后会断开已有中转连接。直连失败不回退，请确认相关工作已保存。', 'warn') : null,
        locked ? h('p', { class: 'warn small' }, testBusy() ? '连接测试进行中，结束后可以连接设备。' : '本机正在接收连接，请先终止该连接。') : null,
        button(batch?.running ? '查看连接进度' : ui.busy.has('connect-all') ? '正在开始…' : '开始连接', batch?.running ? 'all-progress' : 'start-all', { class: 'button primary wide', disabled: !batch?.running && (locked || !peers().length || ui.busy.has('connect-all')) })),
      allConnectionActions());
  }
  function category(item) {
    const known = { components: '设备联系失败', contact: '设备联系失败', direct: '直连未成功', relay: '中转连接失败', ssh: 'SSH 验证失败' };
    if (known[item.stage]) return known[item.stage];
    const source = `${item.stage || ''} ${item.message || ''}`.toLowerCase();
    if (/ssh|登录验证/.test(source)) return 'SSH 验证失败';
    if (/relay|中转/.test(source)) return '中转连接失败';
    if (/direct|直连/.test(source)) return '直连未成功';
    return '设备联系失败';
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
    return h('div', { 'data-key': 'all-progress' }, heading('连接所有设备', batch.running ? '连接正在进行，逐台状态显示在左侧设备列表。' : '本次连接已结束。'),
      h('div', { class: 'progress-line' }, h('span', {}, batch.running ? '正在处理' : '已处理'), h('span', {}, `${Math.min(completed, total)} / ${total} 台`)),
      h('progress', { max: Math.max(total, 1), value: Math.min(completed, total), 'aria-label': '已处理设备数' }),
      h('div', { class: 'metrics' }, metric(direct, '当前直连就绪'), metric(relay, '当前中转就绪'), metric(failed, '本次失败')),
      h('div', { class: 'config-summary' }, summaryLine('跳过', `${items.filter(item => item.state === 'skipped').length} 台`),
        summaryLine('已终止', `${items.filter(item => item.state === 'terminated').length} 台`),
        retainedCount ? summaryLine('保留的中转', `${retainedCount} 台（不计本次直连成功）`) : null),
      failed ? h('section', { class: 'section', 'aria-label': '失败分类' }, h('h3', {}, '失败原因'), [...failures()].map(([label, ids]) =>
        button([label, h('span', {}, `${ids.length} 台`, icon('arrow'))], 'failure-category', { class: 'failure-row', 'data-category': label, 'aria-label': `${label}，${ids.length}台，查看设备` }))) : null,
      !batch.running ? h('div', { class: 'bottom' }, button('重新配置连接', 'all-config', { class: 'button wide' })) : null,
      allConnectionActions());
  }
  function metric(value, label) { return h('div', { class: 'metric' }, h('strong', {}, value), h('span', {}, label)); }
  function configurePage() {
    const item = device(ui.selected); if (!item) return h('p', {}, '设备已不在账号列表中。');
    const existing = sessionFor(item.node_id); const reason = blockReason(item);
    return h('div', { 'data-key': `configure-${item.node_id}` }, h('div', { class: 'backline' }, button('返回设备详情', 'device', { class: 'link' }, 'back')),
      heading(`连接${item.display_name}`, item.purpose || item.os || '选择连接方式后再开始。'),
      existing ? banner('将复用现有连接', '重复连接不会中断已有会话。如需更换连接方式，请先终止现有连接。') : modeOptions(),
      h('p', { class: 'muted' }, '两种模式都需要登录和连接协商。仅限直连限制连接数据路径。'),
      reason ? h('div', { class: 'section warn' }, reason) : null,
      h('div', { class: 'bottom' }, button(ui.busy.has('connect') ? '正在开始…' : existing ? '返回当前连接' : '开始连接', existing ? 'device' : 'start-one', { class: 'button primary wide', 'data-id': item.node_id, disabled: !!reason || ui.busy.has('connect') })));
  }
  function formatTime(value) {
    if (!value) return '';
    const date = new Date(typeof value === 'number' ? (value < 1e12 ? value * 1000 : value) : value);
    return Number.isNaN(date.getTime()) ? '' : date.toLocaleString('zh-CN', { hour12: false });
  }
  function testLinks() {
    return h('div', { class: 'actions', 'data-key': 'test-links' }, button('组件来源与校验', 'components', { class: 'link' }),
      isWindows() ? button('Windows 处理方法', 'windows-help', { class: 'link' }) : null);
  }
  function testExplanation() {
    return h('div', { class: 'test-copy', 'data-key': 'test-explanation' },
      h('p', {}, '检查这台电脑是否可以正常发起和接收连接。'), h('h3', {}, '连接组件与安全软件提示'),
      h('p', {}, '我们使用 FRP 官方发布的连接组件，打包时不修改程序本体。部分安全软件可能将其误报或隔离，导致连接失败。'),
      h('p', {}, '测试过程中可能出现防火墙提示。请核对程序名称和路径，并在防火墙中允许 StarBridge 及其连接组件进行网络通信。'),
      h('p', {}, '如果提示风险，请先核对程序来源及官方文件校验结果。确认属于误报后，再按安全软件提示放行相关文件。'),
      h('div', { class: 'section' }, testLinks()));
  }
  function testPage() {
    const current = test(); const gate = testGate(); const initial = testState() === 'idle';
    const summary = testState() === 'cleanup_failed' ? (current.steps || []).find(step => step.id === 'cleanup')?.message || '测试资源清理未完成，不能显示成功。'
      : current.state === 'passed' && testState() !== 'passed' ? '缺少完整的验证结果，尚不能确认本机连接测试通过。'
      : current.message || stageLabel(current.stage);
    const stepText = { pending: '待执行', running: '进行中', passed: '通过', failed: '未通过', skipped: '未执行', unverified: '尚未验证', cancelled: '已取消' };
    return h('div', { 'data-key': 'test-page' }, heading('本机连接测试', initial ? '测试需要本机空闲，并允许使用中转。' : '显示最近一次测试状态。'),
      initial ? testExplanation() : h('div', { 'data-key': 'test-result' }, banner(testLabel(), message(summary), testTone()),
        (current.steps || []).map(step => h('div', { class: 'stage-row', 'data-key': step.id }, icon(step.state === 'passed' ? 'check' : step.state === 'failed' ? 'close' : 'test'),
          h('div', {}, step.label, step.message ? h('p', {}, message(step.message)) : null), h('span', { class: step.state === 'passed' ? 'good small' : step.state === 'failed' ? 'bad small' : 'muted' }, stepText[step.state] || '尚未验证'))),
        current.finished_at ? h('p', { class: 'muted section' }, `最近一次测试：${formatTime(current.finished_at)}`) : null,
        h('details', { class: 'details', 'data-key': 'test-details' }, h('summary', {}, '测试范围与技术详情'),
          h('p', {}, '使用正式连接组件及隔离的临时 SSH 配置，经中转真实连接回本机，验证登录和固定数据回传。完成清理后才显示最终结果。'),
          h('p', {}, '本测试默认不验证跨设备直连。文件传输未测试。网络超时不代表安全软件已拦截。'),
          current.id ? h('p', {}, `测试 ID：${current.id}`) : null), testLinks()),
      h('div', { class: 'bottom' }, gate ? h('p', { class: 'warn small' }, gate) : null,
        current.state === 'cleaning' ? button('正在清理测试连接…', '', { class: 'button wide', disabled: true })
          : current.state === 'running' ? button('取消测试', 'cancel-test', { class: 'button danger wide', disabled: ui.busy.has('cancel-test') })
          : button(ui.busy.has('start-test') ? '正在开始…' : initial ? '开始连接测试' : '重新测试', 'start-test', { class: 'button primary wide', disabled: !!gate || ui.busy.has('start-test') }),
        !initial && !testBusy() ? button('查看测试说明', 'test-explanation', { class: 'link' }) : null));
  }
  function componentsPage() {
    const info = ui.components;
    return h('div', { 'data-key': 'components-page' }, h('div', { class: 'backline' }, button('返回连接测试', 'test', { class: 'link' }, 'back')),
      heading('组件来源与校验', '官方发布地址和本机实际文件信息。'),
      !info ? h('p', { class: 'muted' }, '正在读取组件信息…') : h('div', {},
        info.version ? h('p', {}, `客户端版本：${info.version}`) : null,
        sourceLink('FRP 官方发布', info.official_url), sourceLink('官方校验文件', info.checksums_url),
        h('p', { class: 'muted section' }, '仅提供下载来源不代表本机文件已通过校验。只有明确比对官方校验值后才显示通过。'),
        (info.files || []).map((file, index) => h('div', { class: 'source-file', 'data-key': `file-${index}` }, h('h3', {}, file.name),
          file.version ? h('p', { class: 'muted' }, `组件版本：${file.version}`) : null,
          h('p', { class: file.verification === 'verified' ? 'good' : ['missing', 'mismatch'].includes(file.verification) ? 'bad' : 'warn' },
            ({ verified: '与发布清单的校验值一致', missing: '文件缺失', mismatch: '与发布清单不一致', not_verified: '尚未完成来源校验' })[file.verification] || '尚未完成来源校验'),
          file.path ? h('p', {}, `本机路径：${file.path}`) : null, file.sha256 ? h('p', {}, `SHA-256：${file.sha256}`) : null,
          file.message ? h('p', { class: 'muted' }, message(file.message)) : null,
          file.source_url ? sourceLink('此文件的发布来源', file.source_url) : null)),
        isWindows() ? h('div', { class: 'section' }, button('Windows 处理方法', 'windows-help', { class: 'link' })) : null));
  }
  function sourceLink(label, url) {
    if (!url) return null;
    return h('div', { class: 'section' }, h('h3', {}, label), h('p', { class: 'command' }, url), button('复制地址', 'copy-url', { class: 'link', 'data-url': url }, 'copy'));
  }
  function fingerprint(ids = ui.exportIds) {
    return readySessions().filter(session => !ids.length || ids.includes(session.target_node_id)).map(session => `${session.session_id}:${session.verified_at || ''}`).sort().join('|');
  }
  function exportPage() {
    const changed = fingerprint() !== ui.exportFingerprint; const data = ui.exportData;
    return h('div', { 'data-key': 'export-page' }, h('div', { class: 'backline' }, button('返回', 'export-back', { class: 'link' }, 'back')),
      heading('复制连接信息给 AI', '仅包含当前已验证且仍活动的 SSH 连接。'),
      changed || ui.syncError ? banner('连接状态已变化', '复制前会重新读取有效连接。已结束的会话命令不再可用。', 'warn') : null,
      h('textarea', { class: 'export-text', readOnly: true, value: changed || ui.syncError ? '连接状态已变化，请重新获取连接信息。' : data?.text || '正在生成连接信息…', 'aria-label': '将复制的连接信息', 'data-key': 'export-preview' }),
      h('p', { class: 'muted' }, '这些命令只适用于发起连接的本机。断开、退出或服务重启后，请重新获取。'),
      h('div', { class: 'bottom actions stretch' }, button(changed ? '重新获取' : '刷新预览', 'refresh-export', { disabled: ui.busy.has('export') || !!ui.syncError }),
        button('复制给 AI', 'copy-export', { class: 'button primary', disabled: ui.busy.has('export') || !!ui.syncError || !fingerprint() }, 'copy')));
  }
  function renderDetail() {
    return ({ device: devicePage, configure: configurePage, all: allPage, test: testPage, components: componentsPage, export: exportPage })[ui.view]?.() || devicePage();
  }
  function header(loggedIn) {
    return h('header', { class: 'app-header', 'data-key': 'app-header' }, h('span', { class: 'brand-mark', 'aria-hidden': 'true' }, 'S'), h('span', {}, 'StarBridge'),
      h('div', { class: 'account' }, loggedIn ? [h('span', { class: 'account-name', title: s().username || '' }, s().username || ''),
        button('退出登录', 'logout', { class: 'link', disabled: ui.busy.has('logout') })] : null),
      frameless ? h('div', { class: 'window-controls', 'aria-label': '窗口操作' },
        button('', 'window-minimize', { class: 'window-button', 'aria-label': '最小化窗口', title: '最小化' }, 'minimize'),
        button('', 'window-close', { class: 'window-button window-close', 'aria-label': '关闭窗口', title: '关闭' }, 'close')) : null);
  }
  function authPage() {
    const registering = ui.authMode === 'register';
    return h('div', { class: 'shell', 'data-key': 'auth-shell' }, header(false), h('main', { class: 'auth' },
      h('form', { class: 'auth-form', 'data-form': 'auth' }, h('h1', {}, registering ? '创建账号' : '登录账号'), h('p', { class: 'intro' }, '登录后，在你的电脑之间建立连接。'),
        field('用户名', 'auth-username', ui.auth.username, { name: 'username', autocomplete: 'username', required: true, maxlength: 80 }),
        field('密码', 'auth-password', ui.auth.password, { name: 'password', type: 'password', autocomplete: registering ? 'new-password' : 'current-password', required: true, minlength: 4 }),
        registering ? field('确认密码', 'auth-confirm', ui.auth.confirm_password, { name: 'confirm_password', type: 'password', autocomplete: 'new-password', required: true, minlength: 4 }) : null,
        h('label', { class: 'remember', 'data-key': 'remember' }, h('input', { id: 'auth-remember', type: 'checkbox', name: 'remember', checked: ui.auth.remember }), '记住登录状态'),
        ui.authError ? h('p', { class: 'auth-error', role: 'alert', 'data-key': 'auth-error' }, ui.authError) : null,
        button(ui.busy.has('auth') ? '正在处理…' : registering ? '注册并登录' : '登录', '', { type: 'submit', class: 'button primary wide', disabled: ui.busy.has('auth') }),
        h('div', { class: 'auth-switch' }, button(registering ? '已有账号，返回登录' : '没有账号？注册', 'auth-switch', { class: 'link', disabled: ui.busy.has('auth') })))));
  }
  function render() {
    if (!s()) {
      patchChildren(root, [h('div', { class: 'shell', 'data-key': 'startup-shell' }, header(false),
        h('main', { class: 'startup', role: 'status' }, ui.loadError || '正在读取登录状态…', ui.loadError ? button('重试', 'retry') : null))]);
      return;
    }
    root.setAttribute('aria-busy', 'false');
    if (!s().logged_in) { patchChildren(root, [authPage()]); return; }
    const content = h('div', { class: 'shell', 'data-key': 'main-shell' }, header(true), h('div', { class: 'workspace' }, sidebar(),
      h('main', { class: `detail${ui.view === 'device' && ui.selected === localId() ? ' local-detail' : ''}`, 'data-key': `detail-${ui.view}-${ui.selected || ''}`, 'aria-label': '设备详情与操作' }, renderDetail())),
      h('footer', { class: 'footbar' }, h('span', { class: ui.syncError || s().stage === 'heartbeat_failed' ? 'bad' : '', title: s().stage === 'heartbeat_failed' ? '暂时无法更新账号设备状态，请检查网络连接。' : undefined },
        ui.syncError ? '状态更新失败，正在重试；复制操作暂不可用。' : s().stage === 'heartbeat_failed' ? '暂时无法更新账号设备状态，正在重试。' : `在线设备 ${peers().filter(item => item.online).length + 1} · SSH 已就绪 ${readySessions().length}`),
        ui.syncError ? button('重新读取', 'retry', { class: 'link' }) : null));
    patchChildren(root, [content]);
  }
  function toast(text) {
    toastNode.textContent = text; toastNode.hidden = false; clearTimeout(ui.toastTimer);
    ui.toastTimer = setTimeout(() => { toastNode.hidden = true; }, 4000);
  }
  function applySnapshot(snapshot) {
    if (!snapshot || typeof snapshot.logged_in !== 'boolean') return false;
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
    if (ui.selected && ui.selected !== snapshot.node_id && !device(ui.selected)) { ui.selected = snapshot.node_id; ui.view = 'device'; }
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
  function openMenu(id, x, y) {
    const item = device(id); if (!item) return;
    const isLocal = id === localId(); const active = !!sessionFor(id) || workingStates.has(operation(id)?.state);
    menu.replaceChildren(...[
      !isLocal ? button(active ? '终止连接' : '连接设备', active ? 'terminate-one' : 'configure', { role: 'menuitem', 'data-id': id, disabled: !active && !!blockReason(item) }) : null,
      button('重命名设备', 'rename', { role: 'menuitem', 'data-id': id }),
    ].filter(Boolean));
    ui.menuNode = id; ui.menuReturn = document.activeElement; menu.hidden = false;
    menu.style.left = `${Math.max(6, Math.min(x, innerWidth - menu.offsetWidth - 6))}px`;
    menu.style.top = `${Math.max(6, Math.min(y, innerHeight - menu.offsetHeight - 6))}px`;
    menu.querySelector('button:not(:disabled)')?.focus();
  }
  function showModal(kind, contents) {
    closeMenu(); ui.modalKind = kind; modal.replaceChildren(...contents); modal.showModal();
  }
  function renameDialog(id) {
    const item = device(id); if (!item) return;
    showModal('rename', [h('h2', { id: 'modal-title' }, '重命名设备'), h('form', { 'data-form': 'rename', 'data-id': id },
      field('设备名称', 'rename-name', item.display_name, { name: 'display_name', maxlength: 80, required: true }),
      h('p', { class: 'muted' }, '名称将同步到账号下的设备列表，当前连接保持不变。'),
      h('div', { class: 'actions' }, button('取消', 'close-modal'), button('保存', '', { type: 'submit', class: 'button primary' })))]);
    const input = document.getElementById('rename-name'); input.focus(); input.select();
  }
  function confirmAll() {
    showModal('terminate', [h('h2', { id: 'modal-title' }, '终止全部连接？'),
      h('p', { class: 'modal-copy' }, '将结束本机发起的全部连接，并取消正在建立的连接和等待任务。现有 SSH 会中断，之后不会自动恢复。'),
      h('div', { class: 'actions' }, button('保留连接', 'close-modal'), button('终止全部连接', 'confirm-terminate-all', { class: 'button danger' }))]);
  }
  function showWindowsHelp() {
    if (!isWindows()) return;
    showModal('windows-help', [h('h2', { id: 'modal-title' }, 'Windows 处理方法'), h('div', { class: 'modal-copy' },
      h('p', {}, '遇到防火墙提示时，先核对 StarBridge 及其连接组件的名称和路径，再允许必要的网络通信。'),
      h('p', {}, '如文件被隔离，可在「Windows 安全中心 → 病毒和威胁防护 → 保护历史记录」查看记录。先核对程序来源和官方校验值，确认属于误报后再按提示放行。'),
      h('p', {}, '如果电脑由组织管理，请联系管理员。不要关闭防护或添加宽泛的文件夹排除项。')),
      h('div', { class: 'actions' }, button('知道了', 'close-modal', { class: 'button primary' }))]);
  }
  async function writeClipboard(text) {
    if (desktop) await desktop.invoke('plugin:clipboard-manager|write_text', { text });
    else await navigator.clipboard.writeText(text);
    toast('已复制到剪贴板');
  }
  async function fetchExport(copy = false) {
    if (ui.busy.has('export')) return;
    ui.busy.add('export'); render();
    try {
      // The backend revalidates readiness at export time, rather than trusting a stale UI snapshot.
      const result = await api('client_export', { form: { target_node_ids: ui.exportIds } });
      ui.exportData = result; ui.exportFingerprint = fingerprint();
      if (copy) {
        if (!result?.items?.length) throw new Error('当前没有可复制的有效 SSH 连接。');
        await writeClipboard(result.text);
      }
    } finally { ui.busy.delete('export'); render(); }
  }
  async function action(target, event) {
    const actionName = target.dataset.action; const id = target.dataset.id;
    if (target.disabled) return;
    if (actionName !== 'menu') closeMenu();
    switch (actionName) {
      case 'window-minimize': case 'window-close':
        if (frameless) await desktop.invoke('client_window_action', { action: actionName === 'window-close' ? 'close' : 'minimize' });
        break;
      case 'retry': await refresh(); break;
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
        await mutate('terminate', 'client_terminate', { form: { target_node_id: id } }); toast('该设备的连接已终止'); break;
      case 'terminate-all': confirmAll(); break;
      case 'confirm-terminate-all': modal.close(); await mutate('terminate', 'client_terminate', { form: { target_node_id: null } }); toast('全部连接及等待任务已终止'); break;
      case 'menu': { const rect = target.getBoundingClientRect(); openMenu(id, rect.right - 155, rect.bottom + 4); break; }
      case 'rename': renameDialog(id); break;
      case 'close-modal': modal.close(); break;
      case 'auth-switch': ui.authMode = ui.authMode === 'login' ? 'register' : 'login'; ui.authError = ''; ui.auth.password = ''; ui.auth.confirm_password = ''; render(); break;
      case 'logout': {
        if (hasWork() || testBusy()) {
          showModal('logout', [h('h2', { id: 'modal-title' }, '退出登录？'), h('p', { class: 'modal-copy' }, '退出将结束本机连接及正在进行的任务，并清除可恢复的登录凭据。'),
            h('div', { class: 'actions' }, button('取消', 'close-modal'), button('退出登录', 'confirm-logout', { class: 'button danger' }))]);
        } else await mutate('logout', 'client_logout');
        break;
      }
      case 'confirm-logout': modal.close(); await mutate('logout', 'client_logout'); break;
      case 'copy-ssh': {
        const result = await api('client_export', { form: { target_node_ids: [id] } });
        const item = result?.items?.find(entry => entry.node_id === id);
        const command = item?.ssh_command || item?.command;
        if (!command) throw new Error('该连接尚未就绪或已经结束，请重新连接后再复制。');
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
      case 'test-explanation': showModal('test-explanation', [h('h2', { id: 'modal-title' }, '连接测试说明'), testExplanation(), h('div', { class: 'actions' }, button('知道了', 'close-modal'))]); break;
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
    if (input.name === 'connection-policy') { ui.policy = input.value; render(); }
    if (input.name === 'relay-choice') { ui.rebuild = input.value === 'rebuild'; render(); }
  });
  document.addEventListener('submit', event => {
    const form = event.target; if (!form.dataset.form) return; event.preventDefault();
    (async () => {
      if (form.dataset.form === 'auth') {
        if (ui.busy.has('auth')) return;
        ui.auth.username = ui.auth.username.trim();
        if (!ui.auth.username) { ui.authError = '请输入用户名。'; render(); return; }
        if (ui.authMode === 'register' && ui.auth.password !== ui.auth.confirm_password) { ui.authError = '两次输入的密码不一致。'; render(); document.getElementById('auth-confirm')?.focus(); return; }
        ui.authError = '';
        try { await mutate('auth', ui.authMode === 'register' ? 'client_register' : 'client_login', { form: { ...ui.auth } }); ui.auth.password = ''; ui.auth.confirm_password = ''; ui.view = 'device'; ui.selected = localId(); render(); }
        catch (error) { ui.authError = message(error); render(); }
      } else if (form.dataset.form === 'metadata' || form.dataset.form === 'rename') {
        const item = device(form.dataset.id); if (!item || ui.busy.has('metadata')) return;
        const rename = form.dataset.form === 'rename'; const values = new FormData(form);
        const draft = rename ? { display_name: String(values.get('display_name') || '').trim(), purpose: item.purpose || '' } : draftFor(item);
        if (!draft.display_name.trim()) throw new Error('设备名称不能为空。');
        const submitted = JSON.stringify(draft);
        await mutate('metadata', 'client_update_device', { form: { node_id: item.node_id, display_name: draft.display_name.trim(), purpose: draft.purpose } });
        if (rename && ui.drafts.has(item.node_id)) {
          ui.drafts.set(item.node_id, { ...ui.drafts.get(item.node_id), display_name: draft.display_name.trim() });
        } else if (JSON.stringify(ui.drafts.get(item.node_id)) === submitted) ui.drafts.delete(item.node_id);
        if (rename) modal.close(); render(); toast('设备信息已保存');
      }
    })().catch(error => toast(message(error)));
  });
  document.addEventListener('keydown', event => {
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
  modal.addEventListener('close', () => { ui.modalKind = null; });
  async function init() {
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
