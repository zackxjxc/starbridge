'use strict';

// Run with: node --test tools/test_client_i18n.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const script = fs.readFileSync(path.join(__dirname, '../client-ui/i18n.js'), 'utf8');

function load({ language = 'zh-CN', saved = null, blocked = false } = {}) {
  const listeners = new Map();
  const storage = new Map(saved === null ? [] : [['starbridge.language', saved]]);
  const window = {
    addEventListener(name, fn) { listeners.set(name, [...(listeners.get(name) || []), fn]); },
    dispatchEvent(event) { for (const fn of listeners.get(event.type) || []) fn(event); },
  };
  const context = { window, navigator: { language, languages: [language] }, document: { documentElement: {} },
    Event: class { constructor(type) { this.type = type; } },
    localStorage: { getItem(key) { if (blocked) throw Error('blocked'); return storage.get(key) ?? null; },
      setItem(key, value) { if (blocked) throw Error('blocked'); storage.set(key, value); } } };
  vm.runInNewContext(script, context);
  return { i18n: window.StarBridgeI18n, context, storage, emit: event => window.dispatchEvent(event) };
}

test('system language resolves Chinese variants and otherwise uses English', () => {
  for (const language of ['zh-CN', 'zh-TW', 'zh-Hans', 'zh']) assert.equal(load({ language }).i18n.locale(), 'zh-CN');
  for (const language of ['en-US', 'en-GB', 'fr-FR', '']) assert.equal(load({ language }).i18n.locale(), 'en');
  assert.equal(load({ saved: 'invalid' }).i18n.preference(), 'system');
});

test('explicit language persists across restart and remains independent of system changes', () => {
  const { i18n, storage, context, emit } = load();
  assert.equal(i18n.setPreference('en'), true);
  assert.equal(storage.get('starbridge.language'), 'en');
  assert.equal(context.document.documentElement.lang, 'en');
  assert.equal(load({ saved: storage.get('starbridge.language') }).i18n.t('登录'), 'Sign in');
  context.navigator.languages = ['zh-TW']; emit({ type: 'languagechange' });
  assert.equal(i18n.locale(), 'en');
  i18n.setPreference('system');
  assert.equal(i18n.locale(), 'zh-CN');
  context.navigator.languages = ['en-US']; emit({ type: 'languagechange' });
  assert.equal(context.document.documentElement.lang, 'en');
  assert.equal(i18n.setPreference('unsupported'), false);
  assert.equal(i18n.preference(), 'system');
});

test('blocked storage does not prevent startup or changing the current language', () => {
  const { i18n } = load({ blocked: true });
  assert.equal(i18n.setPreference('en'), false);
  assert.equal(i18n.t('我的设备'), 'My devices');
});

test('storage changes sync language preferences without resetting application state', () => {
  const { i18n, emit } = load();
  emit({ type: 'storage', key: 'unrelated', newValue: 'en' });
  assert.equal(i18n.preference(), 'system');
  emit({ type: 'storage', key: 'starbridge.language', newValue: 'en' });
  assert.equal(i18n.locale(), 'en');
  emit({ type: 'storage', key: null, newValue: null });
  assert.equal(i18n.preference(), 'system');
});

test('all UI translation calls have English entries and preserve their parameters', () => {
  const { i18n } = load({ saved: 'en' });
  const source = fs.readFileSync(path.join(__dirname, '../client-ui/app.js'), 'utf8');
  const calls = [...source.matchAll(/\bt\(("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')/g)];
  assert.ok(calls.length > 300);
  for (const [, literal] of calls) {
    const key = vm.runInNewContext(literal);
    assert.ok(Object.hasOwn(i18n.english, key), `Missing English entry: ${key}`);
  }
  const slots = text => [...text.matchAll(/\{(\d+)\}/g)].map(match => match[1]).sort().join(',');
  for (const [zh, en] of Object.entries(i18n.english)) {
    assert.ok(en.length > 0, zh);
    assert.equal(slots(zh), slots(en), `Mismatched placeholders: ${zh}`);
    assert.doesNotMatch(en, /[\u3400-\u9fff]/u, `Untranslated English entry: ${zh}`);
  }
  assert.equal(i18n.t('连接{0}', '开发机{1}'), 'Connect to 开发机{1}');
  assert.equal(i18n.t('已被{0}连接', '<script>在线</script>'), 'Connected from <script>在线</script>');
});

test('known service errors translate and can be re-rendered in either language', () => {
  const { i18n } = load({ saved: 'en' });
  const original = '账户不存在或密码不正确，请检查后登录或注册';
  const translated = i18n.message(original);
  assert.match(translated, /incorrect password/);
  assert.equal(i18n.source(translated), original);
  i18n.setPreference('zh-CN');
  assert.equal(i18n.message(translated), original);
  i18n.setPreference('en');
  assert.equal(i18n.message('本地访问端口 22001 启动失败：Connection refused'), 'Could not start local access port 22001: Connection refused');
  assert.equal(i18n.message('SSH 验证失败：C:\\设备\\ssh.exe: exit 255'), 'SSH verification failed: C:\\设备\\ssh.exe: exit 255');
  assert.equal(i18n.message('Unknown technical output {0}'), 'Unknown technical output {0}');
});
