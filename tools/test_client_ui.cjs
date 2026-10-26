'use strict';

// Set NODE_PATH to a directory containing Playwright, then run node tools/test_client_ui.cjs.
// Uses isolated in-memory IPC fixtures; never signs in to the public service.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('playwright');
const assets = path.join(__dirname, '../client-ui');

(async () => {
  const browser = await chromium.launch({ headless: true, ...(process.platform === 'win32' ? { channel: 'msedge' } : {}) });
  try {
    const context = await browser.newContext({ viewport: { width: 900, height: 600 }, locale: 'en-US' });
    await context.addInitScript(() => {
      window.__STARBRIDGE_FRAMELESS__ = true;
      window.__STARBRIDGE_TRAY__ = true;
      window.__desktopLocales = [];
      window.__TAURI__ = { core: { invoke: async (command, args) => {
        if (command === 'client_set_desktop_locale') { window.__desktopLocales.push(args.locale); return; }
        if (command === 'plugin:clipboard-manager|write_text') { window.__copied = args.text; return; }
        if (command === 'client_window_action') return;
        const response = await fetch('/fixture/' + command, { method: 'POST', body: JSON.stringify(args || {}) });
        const value = await response.json();
        if (!response.ok) throw new Error(value.error);
        return value;
      } } };
    });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    let authFailure = false;
    const enrollmentRequests = [];
    const calls = [];
    const state = {
      logged_in: false, username: null, last_username: '', node_id: 'local', display_name: 'Dev-PC',
      purpose: '', platform: 'windows', stage: 'idle', last_error: null, remember_login: true,
      devices: [], sessions: [], device_operations: [], batch: null, self_test: { state: 'idle', steps: [] },
      ssh: { login_user: 'tester' }, allow_relay: true, direct_only: false,
    };
    const local = { node_id: 'local', display_name: 'Dev-PC', purpose: '', os: 'Windows 11', online: true, occupancy: 'idle' };
    const peer = { node_id: 'peer', display_name: 'Build-PC', purpose: '', os: 'Ubuntu 24.04', online: true, occupancy: 'idle', capabilities: ['ssh_server'] };
    const exportData = { text: '后端中文导出', items: [{ name: 'Build-PC', purpose: '保留中文备注', system: 'Ubuntu / x86_64', ssh_user: 'dev', privilege: 'user', path: 'direct', ssh_command: 'ssh dev@localhost -p 22000', source_name: 'Dev-PC', shell: 'PowerShell' }], excluded: [] };
    await context.route('http://starbridge.test/**', async route => {
      const url = new URL(route.request().url());
      if (url.pathname.startsWith('/fixture/')) {
        const command = url.pathname.slice('/fixture/'.length); calls.push(command);
        let value;
        if (command === 'client_config') value = { connection_policy: 'direct_preferred', rebuild_relay: false };
        else if (command === 'client_state') value = state;
        else if (command === 'client_login' || command === 'client_register') {
          if (authFailure) return route.fulfill({ status: 401, json: { error: '账户不存在或密码不正确，请检查后登录或注册' } });
          state.logged_in = true; state.username = 'tester'; state.devices = [local, peer];
          state.device_rebind_required = false; state.enrollment_candidates = []; value = state;
        } else if (command === 'client_resolve_enrollment') {
          const form = route.request().postDataJSON().form;
          assert.deepEqual(Object.keys(form).sort(), ['choice', 'target_node_id']);
          enrollmentRequests.push(form);
          state.device_rebind_required = false; state.enrollment_candidates = [];
          state.node_id = form.choice === 'add' ? 'another-device' : form.target_node_id;
          state.devices = [{ ...local, node_id: state.node_id }, peer]; value = state;
        } else if (command === 'client_connection_history') value = { total: 1, items: [{ session_id: 'history', peer_name: 'Build-PC', direction: 'outgoing', created_at: 1792980000, path: 'direct', status: 'failed', failure_reason: '连接协商已过期' }] };
        else if (command === 'client_components') value = { version: '0.1.0', files: [{ name: 'frpc.exe', verification: 'verified', message: '与发布清单中的文件 SHA-256 一致' }] };
        else if (command === 'client_export') value = exportData;
        else if (command === 'client_logout') { state.logged_in = false; state.username = null; state.sessions = []; value = state; }
        else throw Error('Unexpected mutation: ' + command);
        return route.fulfill({ json: value });
      }
      const name = url.pathname === '/' ? 'index.html' : url.pathname.slice(1);
      if (!['index.html', 'app.js', 'i18n.js', 'style.css', 'tray-notice.html', 'tray-notice.js'].includes(name)) return route.fulfill({ status: 404, body: '' });
      return route.fulfill({ path: path.join(assets, name), contentType: name.endsWith('.js') ? 'text/javascript' : name.endsWith('.css') ? 'text/css' : 'text/html' });
    });
    const choose = async language => {
      await page.locator('#language-trigger').click();
      await page.locator(`[data-language="${language}"]`).click();
      assert.equal(await page.locator('#language-menu').isVisible(), false);
      assert.equal(await page.locator('#language-trigger').getAttribute('aria-expanded'), 'false');
      const locale = language === 'system' ? await page.evaluate(() => window.StarBridgeI18n.systemLocale()) : language;
      assert.equal(await page.evaluate(() => window.__desktopLocales.at(-1)), locale);
      assert.equal(await page.locator('[data-action="window-close"]').getAttribute('title'), locale === 'en' ? 'Close to tray' : '关闭到托盘');
    };
    const noChineseUI = async () => {
      const text = await page.locator('body').evaluate(body => {
        const result = [];
        const walker = document.createTreeWalker(body, NodeFilter.SHOW_TEXT);
        while (walker.nextNode()) {
          const node = walker.currentNode;
          if (!node.parentElement.closest('#language-trigger, #language-menu, script, style') && node.parentElement.getClientRects().length) result.push(node.textContent);
        }
        return result.join('\n');
      });
      assert.doesNotMatch(text, /[\u3400-\u9fff]/u, text);
    };
    const refresh = async () => { await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange'))); await page.waitForTimeout(80); };
    await page.goto('http://starbridge.test/');
    await page.getByRole('heading', { name: 'Sign in', exact: true }).waitFor();
    assert.deepEqual(await page.evaluate(() => window.__desktopLocales), ['en']);
    assert.equal(await page.locator('[data-action="window-close"]').getAttribute('aria-label'), 'Close to tray');
    await noChineseUI();
    await page.locator('#auth-username').fill('tester');
    await page.locator('#auth-password').fill('fixture-password');
    await page.locator('#auth-remember').uncheck();
    const mutationsBefore = calls.filter(call => !['client_state', 'client_config'].includes(call)).length;
    await choose('zh-CN');
    assert.equal(await page.locator('#auth-password').inputValue(), 'fixture-password');
    assert.equal(await page.locator('#auth-username').inputValue(), 'tester');
    assert.equal(await page.locator('#auth-remember').isChecked(), false);
    await page.locator('[data-action="auth-switch"]').click();
    await page.locator('#auth-password').fill('register-password');
    await page.locator('#auth-confirm').fill('register-password');
    await choose('en');
    assert.equal(await page.locator('#auth-confirm').inputValue(), 'register-password');
    assert.equal(calls.filter(call => !['client_state', 'client_config'].includes(call)).length, mutationsBefore);
    await noChineseUI();
    await page.locator('[data-action="auth-switch"]').click();
    await page.locator('#auth-password').fill('fixture-password');
    authFailure = true;
    await page.locator('[data-form="auth"]').evaluate(form => form.requestSubmit());
    await page.getByRole('alert').waitFor();
    assert.match(await page.getByRole('alert').textContent(), /incorrect password/);
    await choose('zh-CN');
    assert.match(await page.getByRole('alert').textContent(), /密码不正确/);
    await choose('en');
    authFailure = false;
    await page.locator('[data-form="auth"]').evaluate(form => form.requestSubmit());
    await page.locator('.list-pane').waitFor();
    await noChineseUI();
    assert.equal(await page.locator('.detail').evaluate(el => el.scrollHeight <= el.clientHeight), true, 'The default home page must fit 900×600');
    if (process.env.UI_SCREENSHOT_DIR) {
      await page.screenshot({ path: path.join(process.env.UI_SCREENSHOT_DIR, 'english-home.png') });
      console.log('Home layout:', await page.locator('.detail').evaluate(el => ({ height: el.clientHeight, content: el.scrollHeight })));
      await page.locator('#language-trigger').click();
      await page.screenshot({ path: path.join(process.env.UI_SCREENSHOT_DIR, 'english-menu.png') });
      await page.keyboard.press('Escape');
    }
    await page.locator('#device-display-name').fill('中文电脑');
    await page.locator('#device-purpose').fill('保留我的备注');
    await page.locator('#device-search').fill('Build');
    const scroll = await page.locator('.detail').evaluate(el => { el.scrollTop = 40; return el.scrollTop; });
    await choose('zh-CN');
    await choose('en');
    assert.equal(await page.locator('#device-display-name').inputValue(), '中文电脑');
    assert.equal(await page.locator('#device-purpose').inputValue(), '保留我的备注');
    assert.equal(await page.locator('#device-search').inputValue(), 'Build');
    assert.equal(await page.locator('.detail').evaluate(el => el.scrollTop), scroll);
    await page.locator('#language-trigger').click();
    await page.keyboard.press('End');
    assert.equal(await page.locator('[data-language="en"]').evaluate(el => el === document.activeElement), true);
    await page.keyboard.press('Escape');
    assert.equal(await page.locator('#language-trigger').evaluate(el => el === document.activeElement), true);
    await page.locator('#language-trigger').click();
    await page.locator('.list-title h2').click();
    assert.equal(await page.locator('#language-menu').isVisible(), false);
    await page.locator('[data-action="history"]').click();
    await page.locator('[data-action="history-details"]').waitFor();
    await page.locator('[data-action="history-direction"][data-direction="outgoing"]').click();
    await page.locator('[data-action="history-details"]').click();
    await choose('zh-CN'); await choose('en');
    assert.equal(await page.locator('[data-direction="outgoing"]').getAttribute('aria-pressed'), 'true');
    assert.equal(await page.locator('[data-action="history-details"]').getAttribute('aria-expanded'), 'true');
    await noChineseUI();
    await page.locator('[data-action="device"]').click();
    await page.locator('[data-action="all"]').click();
    await noChineseUI();
    await page.locator('.device-row[data-id="local"]').click();
    await page.locator('[data-action="test"]').first().click();
    await noChineseUI();
    await page.locator('[data-action="components"]').click();
    await page.locator('.source-file').waitFor();
    await noChineseUI();
    await page.locator('[data-action="windows-help"]').click();
    await noChineseUI();
    await page.evaluate(() => window.StarBridgeI18n.setPreference('zh-CN'));
    assert.match(await page.locator('#modal-title').textContent(), /Windows 处理方法/);
    await page.evaluate(() => window.StarBridgeI18n.setPreference('en'));
    await page.locator('#modal [data-action="close-modal"]').click();
    state.self_test = { state: 'passed', stage: 'ready', finished_at: 1792980000, message: '本机连接测试通过：SSH 发起、SSHD 接收、服务器中转均正常。跨设备直连将在实际连接时检查。', steps: ['components', 'relay', 'ssh', 'sshd', 'cleanup'].map((id, i) => ({ id, label: ['连接组件', '服务器中转', 'SSH 发起', 'SSHD 接收', '资源清理'][i], state: 'passed', message: '正式连接组件已启动' })) };
    await refresh();
    await page.locator('[data-action="test"]').first().click();
    await noChineseUI();
    state.sessions = [{ session_id: 'live', initiator_node_id: 'local', target_node_id: 'peer', status: 'connected', path: 'direct', ssh_ready: true, verified_at: 1792980000, ssh_command: 'ssh dev@localhost -p 22000' }];
    await refresh();
    await page.locator('.device-row[data-id="peer"]').click();
    await page.locator('[data-action="export-one"]').click();
    await page.waitForFunction(() => document.querySelector('.export-text')?.value.startsWith('Device:'));
    const englishExport = await page.locator('.export-text').inputValue();
    assert.match(englishExport, /Purpose: 保留中文备注/);
    assert.match(englishExport, /SSH: ssh dev@localhost -p 22000/);
    await choose('zh-CN');
    assert.match(await page.locator('.export-text').inputValue(), /设备：Build-PC/);
    await choose('en');
    await page.locator('[data-action="copy-export"]').click();
    await page.waitForFunction(() => window.__copied?.startsWith('Device:'));
    assert.equal(await page.evaluate(() => window.__copied), englishExport);
    await noChineseUI();
    await page.locator('[data-action="logout"]').click();
    await page.locator('#modal [data-action="confirm-logout"]').click();
    await page.locator('[data-form="auth"]').waitFor();
    assert.equal(await page.locator('html').getAttribute('lang'), 'en');
    await page.reload();
    await page.locator('[data-form="auth"]').waitFor();
    assert.equal(await page.evaluate(() => localStorage.getItem('starbridge.language')), 'en');
    await page.locator('#language-trigger').click();
    const menuBox = await page.locator('#language-menu').boundingBox();
    assert.ok(menuBox.x >= 0 && menuBox.x + menuBox.width <= 900 && menuBox.y + menuBox.height <= 600);
    await page.keyboard.press('Escape');
    await page.emulateMedia({ colorScheme: 'dark' });
    await noChineseUI();
    await choose('system');
    await page.evaluate(() => {
      Object.defineProperty(navigator, 'languages', { configurable: true, value: ['zh-TW'] });
      window.dispatchEvent(new Event('languagechange'));
    });
    assert.equal(await page.evaluate(() => window.__desktopLocales.at(-1)), 'zh-CN');
    assert.equal(await page.locator('[data-action="window-close"]').getAttribute('title'), '关闭到托盘');
    await choose('en');

    // A verified account resolves a live-instance choice without a second password.
    Object.assign(state, { logged_in: true, username: 'tester', node_id: 'local', devices: [local, peer],
      device_rebind_required: true, enrollment_candidates: [{ node_id: 'local', display_name: 'Dev-PC',
        online: true, active_connections: 2, test_running: true }] });
    await page.reload();
    await page.locator('[data-action="rebind-device"]').click();
    assert.equal(await page.locator('#modal input[type="password"]').count(), 0);
    assert.match(await page.locator('#modal').innerText(), /Active connections: 2/);
    await noChineseUI();
    await page.screenshot({ path: path.join(__dirname, '../.cache/device-enrollment-review/enrollment-choice.png') });
    await page.locator('[data-action="add-instance"]').click();
    await page.waitForFunction(() => !document.querySelector('#modal').open);
    assert.deepEqual(enrollmentRequests.at(-1), { choice: 'add', target_node_id: null });

    Object.assign(state, { node_id: 'local', device_rebind_required: true,
      enrollment_candidates: [{ node_id: 'local', display_name: 'Dev-PC', online: true, active_connections: 2, test_running: true }] });
    await page.reload();
    await page.locator('[data-action="rebind-device"]').click();
    await page.locator('[data-action="replace-instance"]').click();
    assert.equal(await page.locator('#modal input[type="password"]').count(), 0);
    assert.match(await page.locator('#modal').innerText(), /2 active connections/);
    await page.locator('[data-action="confirm-replace-instance"]').click();
    await page.waitForFunction(() => !document.querySelector('#modal').open);
    assert.deepEqual(enrollmentRequests.at(-1), { choice: 'replace', target_node_id: 'local' });

    Object.assign(state, { device_rebind_required: true, enrollment_candidates: [] });
    await page.reload();
    await page.locator('[data-form="auth"]').waitFor();
    assert.equal(await page.locator('input[type="password"]').count(), 1);
    assert.match(await page.locator('body').innerText(), /automatically restore this computer/);

    const notice = await context.newPage();
    await notice.setViewportSize({ width: 360, height: 138 });
    await notice.addInitScript(() => {
      window.__STARBRIDGE_TRAY_NOTICE__ = { locale: 'en', title: 'StarBridge is running in the background',
        body: 'Connections stay active. Click the tray icon to reopen, or right-click and choose Exit to quit.' };
    });
    await notice.goto('http://starbridge.test/tray-notice.html');
    assert.equal(await notice.locator('html').getAttribute('lang'), 'en');
    assert.equal(await notice.locator('body').isVisible(), true);
    assert.doesNotMatch(await notice.locator('body').innerText(), /[\u3400-\u9fff]/u);
    assert.equal(await notice.evaluate(() => document.body.scrollHeight <= innerHeight), true, 'English tray notice must fit its window');
    await notice.evaluate(() => window.renderTrayNotice({ locale: 'zh-CN', title: 'StarBridge 已在后台运行',
      body: '连接保持不变。点击托盘图标打开窗口，右键选择“退出”结束程序。' }));
    assert.equal(await notice.locator('html').getAttribute('lang'), 'zh-CN');
    assert.match(await notice.locator('#notice-body').textContent(), /连接保持不变/);
    await notice.close();
    assert.deepEqual(errors, []);
    await context.close();
    console.log('PASS: 900×600 desktop chrome, login/register, service errors, language persistence, drafts/search/scroll, keyboard/outside click, history filters, configuration, diagnostics, dialogs, export preview and clipboard, sign-out, reload, dark theme');
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
