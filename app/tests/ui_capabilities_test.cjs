// Run with: node app/tests/ui_capabilities_test.cjs
// Actual inline UI code, deterministic clock/network, no browser or external CLI required.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const {webcrypto} = require('node:crypto');
const html = fs.readFileSync(require('node:path').resolve(__dirname, '../static/index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const clone = value => JSON.parse(JSON.stringify(value));
const settle = () => new Promise(resolve => setImmediate(resolve));
const capability = (state = 'unknown', reason = 'fixture reason', source = 'fixture source') => ({state, reason, source});
const model = (id, supported_efforts) => ({id, model: id, display_name: 'Model ' + id, description: 'description ' + id, default_effort: null, supported_efforts, is_default: null, hidden: null, source: 'codex_app_server.model/list'});
const catalog = (version = 'fixture-1') => ({provider: 'codex_app_server', checked_at_unix_ms: 1700000000000, cli_version: version,
  executable: capability('supported', 'configured executable is runnable'), compatibility: capability('supported', 'protocol verified'), authentication: capability('unknown', 'login not checked'), reviewer_isolation: capability('unsupported', 'read-only isolation unavailable'), permission_control: capability('supported'), session_continuity: capability('unknown'), startup_context: capability('unknown', 'isolated discovery context differs from execution'), model_catalog: capability('supported'),
  models: [model('alpha', [{effort: 'low', description: 'fast'}, {effort: 'high', description: null}]), model('unknown-efforts', null), model('empty-efforts', [])],
  selection: {requested_model: 'manual-model', requested_effort: 'high', effective_model: null, effective_effort: null, status: capability('unknown', 'execution has not verified this selection')}});
const envelope = (name = 'dev / one', generation = 1, data = catalog(), stale = false, refreshing = false, cache_epoch = 'process-a') => ({name, cache_epoch, generation, catalog: data, stale, refreshing});
function fixture(authMode = 'bearer', restored = false) {
  const nodes = new Map(), events = new Map(), timers = new Map(); let active = null, timerId = 0;
  class Element {
    constructor(tag = 'div', id = '') { this.tagName = tag.toUpperCase(); this.id = id; this.children = []; this.dataset = {}; this.attrs = {}; this.events = new Map(); this.hidden = false; this.disabled = false; this.open = false; this._text = ''; this._value = ''; this.className = ''; this.classList = {toggle: () => {}}; }
    get value() { return this._value; } set value(value) { this._value = String(value); }
    get textContent() { return this._text + this.children.map(child => child.textContent || '').join(''); } set textContent(value) { this._text = String(value); this.children = []; }
    set innerHTML(_) { throw new Error('Unsafe HTML rendering'); }
    append(...children) { this.children.push(...children); if (this.tagName === 'SELECT' && this.children.length === children.length) this.value = children[0]?.value || ''; }
    replaceChildren(...children) { this._text = ''; this.children = []; this.append(...children); }
    setAttribute(key, value) { this.attrs[key] = String(value); } getAttribute(key) { return this.attrs[key] ?? null; }
    addEventListener(name, fn) { this.events.set(name, [...this.events.get(name) || [], fn]); } removeEventListener() {}
    focus() { active = this; } showModal() { this.open = true; } close() { this.open = false; return this.emit('close'); }
    querySelectorAll(tag) { return this.children.flatMap(child => [...(child.tagName === tag.toUpperCase() ? [child] : []), ...child.querySelectorAll(tag)]); }
    emit(name, extra = {}) { return Promise.all((this.events.get(name) || []).map(fn => fn({preventDefault() {}, ...extra}))); }
  }
  for (const match of html.matchAll(/<([a-z0-9]+)\b([^>]*\bid="([^"]+)"[^>]*)>/g)) { const node = new Element(match[1], match[3]); node.hidden = /\bhidden\b/.test(match[2]); node.disabled = /\bdisabled\b/.test(match[2]); nodes.set(match[3], node); }
  const filters = ['all', 'queued', 'claimed', 'finished'].map(filter => { const node = new Element('button'); node.dataset.filter = filter; return node; });
  const document = {getElementById: id => { assert(nodes.has(id), id); return nodes.get(id); }, createElement: tag => new Element(tag), querySelectorAll: () => filters, documentElement: new Element('html'), get activeElement() { return active; }};
  const f = {nodes, events, timers, requests: [], pending: [], authenticated: restored, delayRead: false, delayRefresh: false, delayConfig: false, delayLogout: false, ignoreAbort: false, readFailure: null, refreshFailure: null, refreshResponse: null,
    config: {repositories: ['repo'], agents: ['generic', 'dev / one', 'review'], tests: ['test'], native_agents: [{name: 'dev / one', provider: 'codex_app_server', model: 'manual-model', effort: 'high'}, {name: 'review', provider: 'claude_cli', model: null, effort: null}]},
    cache: {profiles: [envelope(), envelope('review', 0, null, true)]}};
  const $ = id => nodes.get(id); f.$ = $;
  f.card = name => $('capability-profiles').children.find(node => node.dataset.profile === name);
  f.button = name => f.card(name)?.querySelectorAll('button')[0];
  f.catalogRequests = () => f.requests.filter(request => request.url.startsWith('/api/capabilities'));
  f.posts = () => f.requests.filter(request => request.opts.method === 'POST' && request.url.startsWith('/api/capabilities'));
  f.flushTimer = async milliseconds => { for (const [id, timer] of [...timers]) if (timer.milliseconds === milliseconds) { timers.delete(id); timer.callback(); } await settle(); };
  f.resolve = (url, method = 'GET') => { const index = f.pending.findIndex(item => item.url === url && item.method === method); assert(index >= 0, 'missing pending ' + method + ' ' + url); f.pending.splice(index, 1)[0].resolve(); };
  f.login = async () => { $('token').value = 'test-token'; $('username').value = 'operator'; $('password').value = 'password'; await $('auth-form').emit('submit'); await settle(); };
  async function fetch(url, opts) {
    f.requests.push({url, opts}); assert.equal(opts.redirect, 'error'); assert.equal(opts.credentials, authMode === 'bearer' && url.startsWith('/api/') ? 'omit' : 'same-origin');
    assert.equal(opts.headers.Authorization, authMode === 'bearer' && url.startsWith('/api/') ? 'Bearer test-token' : undefined);
    let data, status = 200, delayed = false;
    if (url === '/auth/status') data = {mode: authMode, authenticated: f.authenticated};
    else if (url === '/auth/login') { f.authenticated = true; data = {authenticated: true}; }
    else if (url === '/auth/logout') { f.authenticated = false; data = {authenticated: false}; delayed = f.delayLogout; }
    else if (url === '/api/config') { data = f.config; delayed = f.delayConfig; }
    else if (url === '/api/tasks') data = [];
    else if (url === '/api/status') data = {active: null, recovery_required: false, diagnostic: null};
    else if (url === '/api/capabilities') { data = f.cache; delayed = f.delayRead; if (f.readFailure) { status = f.readFailure; data = {error: 'cache unavailable'}; } }
    else if (url.startsWith('/api/capabilities/') && url.endsWith('/refresh')) {
      assert.equal(opts.method, 'POST'); assert.equal(opts.body, undefined);
      const name = decodeURIComponent(url.slice('/api/capabilities/'.length, -'/refresh'.length));
      data = f.refreshResponse || envelope(name, 2, catalog('fixture-2')); delayed = f.delayRefresh;
      if (f.refreshFailure) { status = f.refreshFailure; data = {error: 'another catalog refresh is in progress'}; }
    } else throw new Error('Unexpected endpoint ' + url);
    const snapshot = clone(data), response = {ok: status === 200, status, json: async () => clone(snapshot)};
    if (delayed) return new Promise((resolve, reject) => { f.pending.push({url, method: opts.method, signal: opts.signal, resolve: () => resolve(response)}); if (!f.ignoreAbort) opts.signal.addEventListener('abort', () => { const error = new Error('abort'); error.name = 'AbortError'; reject(error); }); });
    return response;
  }
  const context = {console, document, window: {matchMedia: () => ({matches: false, addEventListener() {}}), addEventListener: (name, fn) => events.set(name, fn)}, navigator: {}, fetch, crypto: webcrypto, AbortController, TextEncoder, Uint8Array, Date, Error, JSON, Array, String, Number, Boolean, Set, Map, encodeURIComponent,
    setTimeout: (callback, milliseconds) => { const id = ++timerId; timers.set(id, {callback, milliseconds}); return id; }, clearTimeout: id => timers.delete(id)};
  Object.defineProperties(context, {localStorage: {get() { throw new Error('must not access storage'); }}, sessionStorage: {get() { throw new Error('must not access storage'); }}});
  vm.runInNewContext(script, context); return f;
}
(async () => {
  for (const authMode of ['bearer', 'session', 'hybrid']) {
    const f = fixture(authMode), $ = f.$; await settle();
    assert($('capability-panel').hidden); assert.equal(f.catalogRequests().length, 0);
    await f.login(); assert(!$('capability-panel').hidden); assert($('capability-body').hidden); assert.equal(f.catalogRequests().length, 1); assert.equal(f.posts().length, 0);
    assert.match($('capability-summary').textContent, /2 个/); assert.equal($('agent').value, 'generic');
    await $('capability-toggle').emit('click'); assert.equal(f.catalogRequests().length, 2); assert.equal($('capability-toggle').getAttribute('aria-expanded'), 'true');
    const content = f.card('dev / one').textContent;
    for (const text of ['fixture-1', '发现启动上下文', 'isolated discovery context differs from execution', '支持', '不支持', '未知', 'fixture reason', 'fixture source', 'manual-model', 'high', '未验证', '未知（尚无执行证据）', 'login not checked', '来源：codex_app_server.model/list', 'low（fast）', '支持的 effort：未知（供应商未提供）', '空列表（未报告可选项）']) assert(content.includes(text), text);
    assert.match(f.card('review').textContent, /尚无缓存/); assert.match(f.card('review').textContent, /未指定（由 CLI 决定）/);
    assert.equal($('capability-profiles').querySelectorAll('select').length, 0); assert.equal($('capability-profiles').querySelectorAll('input').length, 0);
    // Task refresh, scheduled task polling, and online notifications never discover or read catalogs.
    const beforePolling = f.catalogRequests().length;
    await $('refresh').emit('click'); await f.flushTimer(2000); f.events.get('online')(); await settle();
    assert.equal(f.catalogRequests().length, beforePolling); assert.equal(f.posts().length, 0);
    // Safe literal profile/model/evidence text, stale state, and ignored unconfigured profiles.
    const attack = '<img src=x onerror=alert(1)>', injected = catalog('fixture-safe'); injected.models[0].display_name = attack; injected.models[0].supported_efforts[0].description = attack; injected.authentication = capability('unknown', attack, attack);
    f.cache = {profiles: [envelope('dev / one', 3, injected, true), envelope('review', 0, null, true), envelope('unconfigured', 99)]}; await $('capability-read').emit('click');
    assert.match(f.card('dev / one').textContent, /缓存已陈旧/); assert(f.card('dev / one').textContent.includes(attack)); assert.equal($('capability-profiles').querySelectorAll('img').length, 0); assert.equal($('capability-profiles').children.length, 2);
    // A lower-generation read cannot replace newer evidence.
    f.cache = {profiles: [envelope('dev / one', 2, catalog('too-old'))]}; await $('capability-read').emit('click'); assert.match(f.card('dev / one').textContent, /fixture-safe/); assert(!f.card('dev / one').textContent.includes('too-old'));
    // Exactly one explicit profile POST, with no request body or task/config mutation.
    f.delayRefresh = true; f.refreshResponse = envelope('dev / one', 4, catalog('fresh-discovery'));
    const original = f.button('dev / one'), discovering = original.emit('click'); await settle(); await original.emit('click'); await f.button('dev / one').emit('click');
    assert.equal(f.posts().length, 1); assert.equal(f.posts()[0].url, '/api/capabilities/dev%20%2F%20one/refresh'); assert(f.button('dev / one').disabled);
    // A cache read begun during discovery may return after completion; it must not roll it back.
    f.delayRead = true; f.ignoreAbort = true; f.cache = {profiles: [envelope('dev / one', 4, catalog('old-during-refresh'), true, true)]};
    const during = $('capability-read').emit('click'); await settle(); f.resolve('/api/capabilities/dev%20%2F%20one/refresh', 'POST'); await discovering;
    f.resolve('/api/capabilities'); await during; assert.match(f.card('dev / one').textContent, /fresh-discovery/); assert(!f.button('dev / one').disabled);
    // Starting a new discovery fences an already outstanding read, even if transport ignores abort.
    f.cache = {profiles: [envelope('dev / one', 90, catalog('stale-read'))]}; const oldRead = $('capability-read').emit('click'); await settle();
    f.refreshResponse = envelope('dev / one', 5, catalog('new-discovery')); const newer = f.button('dev / one').emit('click'); await settle();
    f.resolve('/api/capabilities/dev%20%2F%20one/refresh', 'POST'); await newer; f.resolve('/api/capabilities'); await oldRead; assert.match(f.card('dev / one').textContent, /new-discovery/); assert(!f.card('dev / one').textContent.includes('stale-read'));
    f.delayRead = false; f.delayRefresh = false; f.ignoreAbort = false;
    // Another caller's in-progress discovery requires an explicit cache read, never an automatic retry.
    f.refreshResponse = envelope('dev / one', 6, catalog('new-discovery'), true, true); await f.button('dev / one').emit('click'); assert(f.button('dev / one').disabled);
    const posts = f.posts().length; await f.flushTimer(2000); assert.equal(f.posts().length, posts);
    f.cache = {profiles: [envelope('dev / one', 6, catalog('other-completed'))]}; await $('capability-read').emit('click'); assert(!f.button('dev / one').disabled); assert.match(f.card('dev / one').textContent, /other-completed/);
    // Discovery conflict and read failure stay local and retain usable task controls/evidence.
    f.refreshFailure = 409; await f.button('review').emit('click'); assert.match(f.card('review').textContent, /another catalog refresh/); assert($('network-banner').hidden); assert(!$('submit-task').disabled); f.refreshFailure = null;
    f.readFailure = 500; await $('capability-read').emit('click'); assert(!$('capability-error').hidden); assert.match(f.card('dev / one').textContent, /other-completed/); assert($('network-banner').hidden); f.readFailure = null;
    // Malformed or wrong-profile refresh replies never replace valid evidence.
    f.refreshResponse = envelope('review', 100, catalog('wrong-profile')); await f.button('dev / one').emit('click'); assert.match(f.card('dev / one').textContent, /结果不匹配/); assert.match(f.card('dev / one').textContent, /other-completed/);
    f.cache = {profiles: [envelope('dev / one'), envelope('dev / one')]}; await $('capability-read').emit('click'); assert.match($('capability-error').textContent, /格式不符合预期/);
    // Close aborts cached reads, and a late response cannot reopen the panel or replace the next open.
    f.delayRead = true; f.ignoreAbort = true; f.cache = {profiles: [envelope('dev / one', 100, catalog('closed-read'))]}; const closingRead = $('capability-read').emit('click'); await settle(); await $('capability-toggle').emit('click'); assert($('capability-body').hidden); assert(f.pending[0].signal.aborted);
    f.cache = {profiles: [envelope('dev / one', 7, catalog('reopened-read'))]}; const reopened = $('capability-toggle').emit('click'); await settle();
    f.pending[1].resolve(); f.pending.splice(1, 1); await reopened; f.resolve('/api/capabilities'); await closingRead; assert.match(f.card('dev / one').textContent, /reopened-read/); assert(!f.card('dev / one').textContent.includes('closed-read'));
    f.delayRead = false;
    // Catalog auth expiry clears profiles, closes the panel, and retains no automatic discovery.
    f.refreshFailure = 401; await f.button('dev / one').emit('click'); assert(!$('auth-panel').hidden); assert($('capability-panel').hidden); assert($('capability-body').hidden); assert.equal($('capability-profiles').children.length, 0); f.refreshFailure = null;
    await f.login(); assert($('capability-body').hidden); assert.equal(f.posts().length, posts + 3); // conflict, wrong reply, and auth-expired attempts only
    // Logout fences an in-flight discovery even when abort is ignored and a new login has happened.
    f.delayRefresh = true; f.refreshResponse = envelope('dev / one', 100, catalog('logged-out-result')); const oldDiscovery = f.button('dev / one').emit('click'); await settle();
    await $('logout').emit('click'); assert($('capability-panel').hidden); assert.equal($('capability-profiles').children.length, 0); await f.login();
    f.resolve('/api/capabilities/dev%20%2F%20one/refresh', 'POST'); await oldDiscovery; assert(!f.card('dev / one').textContent.includes('logged-out-result')); assert($('capability-body').hidden);
    f.delayRefresh = false; f.ignoreAbort = false;
    assert.equal($('agent').value, 'generic'); assert.equal($('requirements').value, ''); assert(!f.requests.some(request => request.opts.method === 'POST' && request.url === '/api/tasks'));
    f.events.get('pagehide')(); f.events.get('pageshow')({persisted: true}); await settle(); assert($('capability-body').hidden); assert.equal($('capability-toggle').getAttribute('aria-expanded'), 'false');
    await $('logout').emit('click'); const stopped = f.requests.length; await f.flushTimer(2000); assert.equal(f.requests.length, stopped);
  }
  // A bearer page can outlive the server. A new cache epoch resets generation and
  // releases a previously refreshing profile instead of remaining locked forever.
  const restarted = fixture(); await settle();
  restarted.cache = {profiles: [envelope('dev / one', 40, catalog('pre-restart'), true, true), envelope('review', 8, catalog('pre-restart-review'))]}; await restarted.login();
  assert(restarted.button('dev / one').disabled);
  restarted.cache = {profiles: [envelope('dev / one', 0, null, true, false, 'process-b')]}; await restarted.$('capability-read').emit('click');
  assert(!restarted.button('dev / one').disabled); assert.match(restarted.card('dev / one').textContent, /尚无缓存/); assert(!restarted.$('capability-profiles').textContent.includes('pre-restart')); assert.match(restarted.card('review').textContent, /尚无缓存/);
  // A known retired epoch cannot come back, even with a larger generation.
  restarted.cache = {profiles: [envelope('dev / one', 999, catalog('retired-cache'), true, true)]}; await restarted.$('capability-read').emit('click');
  assert(!restarted.button('dev / one').disabled); assert(!restarted.card('dev / one').textContent.includes('retired-cache'));
  // Discovery from the old process may resolve after both the new cache read and
  // a new explicit refresh. Its finally handler must not unlock the newer refresh.
  restarted.delayRefresh = true; restarted.ignoreAbort = true; restarted.refreshResponse = envelope('dev / one', 1, catalog('old-inflight'), false, false, 'process-b');
  const oldProcessRefresh = restarted.button('dev / one').emit('click'); await settle();
  restarted.cache = {profiles: [envelope('dev / one', 0, null, true, false, 'process-c')]}; await restarted.$('capability-read').emit('click'); assert(!restarted.button('dev / one').disabled);
  restarted.refreshResponse = envelope('dev / one', 1, catalog('new-process'), false, false, 'process-c'); const newProcessRefresh = restarted.button('dev / one').emit('click'); await settle();
  restarted.resolve('/api/capabilities/dev%20%2F%20one/refresh', 'POST'); await oldProcessRefresh; assert(restarted.button('dev / one').disabled); assert(!restarted.card('dev / one').textContent.includes('old-inflight'));
  restarted.resolve('/api/capabilities/dev%20%2F%20one/refresh', 'POST'); await newProcessRefresh; assert(!restarted.button('dev / one').disabled); assert.match(restarted.card('dev / one').textContent, /new-process/); assert.match(restarted.card('dev / one').textContent, /上次读取时缓存有效/);
  // Mixed process snapshots are malformed; previous good state stays intact.
  restarted.cache = {profiles: [envelope('dev / one', 2, null, true, false, 'process-c'), envelope('review', 0, null, true, false, 'process-d')]}; await restarted.$('capability-read').emit('click'); assert.match(restarted.$('capability-error').textContent, /格式不符合预期/); assert.match(restarted.card('dev / one').textContent, /new-process/); restarted.events.get('pagehide')();
  // Interrupted configuration loading never exposes a catalog or launches discovery.
  const f = fixture(); await settle(); f.delayConfig = true; f.ignoreAbort = true; const interrupted = f.login(); await settle(); f.events.get('pageshow')({persisted: true}); f.resolve('/api/config'); await interrupted;
  assert(f.$('capability-panel').hidden); assert.equal(f.catalogRequests().length, 0);
  // Legacy configs without native_agents keep working and never call the discovery API.
  const legacy = fixture(); await settle(); delete legacy.config.native_agents; await legacy.login(); await legacy.$('capability-toggle').emit('click'); assert.match(legacy.$('capability-profiles').textContent, /未配置原生 Agent/); assert.equal(legacy.catalogRequests().length, 0); assert(legacy.$('capability-read').disabled); legacy.events.get('pagehide')();
  // Cookie restoration reads only the cache and never initiates discovery.
  for (const authMode of ['session', 'hybrid']) { const restored = fixture(authMode, true); await settle(); assert.equal(restored.catalogRequests().length, 1); assert.equal(restored.posts().length, 0); assert(!restored.$('capability-panel').hidden); restored.events.get('pagehide')(); }
  assert.match(html, /能力与模型目录不代表账户已登录、可调用模型或获得调用授权/);
  console.log('PASS: capability UI cached-only login/open; explicit bounded-profile refresh; no polling discovery; process epoch/generation/session/navigation fences; restart unlock and stale-finally guard; repeated click guard; safe evidence/models/efforts; unknown auth and effective selection; startup context; last-read freshness; local errors; legacy config; bearer/session/hybrid restoration');
})().catch(error => { console.error(error); process.exitCode = 1; });
