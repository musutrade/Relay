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
const observation = (extra = {}) => ({task_id: 42, repository: 'task-repo', role: 'reviewer', cli_version: '2.1.291', checked_at_unix_ms: 1700000010000, requested_model: 'task-requested-alias', requested_effort: 'high', native_permission: 'claude_restricted', models: [
  {...model('future-route-from-cli', [{effort: 'future-effort', description: 'supplied dynamically'}]), source: 'claude_cli:task_initialize', resolved_model: 'resolved-future-route', supports_effort: true, supports_adaptive_thinking: true, supports_fast_mode: false, supports_auto_mode: false},
  {...model('legacy-model-fields', null), source: 'claude_cli:task_initialize'},
], ...extra});
const startupText = '启动可能执行正常 managed/user hooks、policy/auth helpers、MCP/plugins、访问网络并产生费用；Relay 仅发送 initialize，不发送模型任务提示，不保证没有副作用。<img src=x onerror=alert(1)>';
const startupEnvelope = (token, expires, extra = {}) => ({...envelope('review', 1, null, true), startup_discovery: {confirmation_token: token, expires_at_unix_ms: expires, confirmation_text: startupText}, ...extra});
const observedEnvelope = (data = observation(), stale = false, generation = 0, standalone = null, catalogStale = true) => ({...envelope('review', generation, standalone, catalogStale), task_observation: data, task_observation_stale: stale});
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
  const f = {nodes, events, timers, now: Date.now(), startupStarts: 0, validateStartup: false, networkRefreshFailure: false, refreshErrorMessage: null, requests: [], pending: [], authenticated: restored, delayRead: false, delayRefresh: false, delayConfig: false, delayLogout: false, ignoreAbort: false, readFailure: null, refreshFailure: null, refreshResponse: null,
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
      assert.equal(opts.method, 'POST');
      if (!f.validateStartup) assert.equal(opts.body, undefined);
      const name = decodeURIComponent(url.slice('/api/capabilities/'.length, -'/refresh'.length));
      data = f.refreshResponse || envelope(name, 2, catalog('fixture-2')); delayed = f.delayRefresh;
      if (f.validateStartup) {
        const body = opts.body ? JSON.parse(opts.body) : null, scope = f.cache.profiles.find(entry => entry.name === name)?.startup_discovery;
        if (!body?.confirm_startup_effects || !scope?.confirmation_token || body.confirmation_token !== scope.confirmation_token || scope.expires_at_unix_ms <= f.now) { status = 409; data = {error: 'startup confirmation expired, replayed or profile/executable/policy drifted'}; }
        else if (!f.refreshFailure) { f.startupStarts++; scope.confirmation_token = null; scope.expires_at_unix_ms = null; }
      }
      if (f.refreshFailure) { status = f.refreshFailure; data = {error: f.refreshErrorMessage || 'another catalog refresh is in progress'}; }
      if (f.networkRefreshFailure) throw new Error('connection lost after startup request');
    } else throw new Error('Unexpected endpoint ' + url);
    const snapshot = clone(data), response = {ok: status === 200, status, json: async () => clone(snapshot)};
    if (delayed) return new Promise((resolve, reject) => { f.pending.push({url, method: opts.method, signal: opts.signal, resolve: () => resolve(response)}); if (!f.ignoreAbort) opts.signal.addEventListener('abort', () => { const error = new Error('abort'); error.name = 'AbortError'; reject(error); }); });
    return response;
  }
  class FixtureDate extends Date { static now() { return f.now; } }
  const context = {console, document, window: {matchMedia: () => ({matches: false, addEventListener() {}}), addEventListener: (name, fn) => events.set(name, fn)}, navigator: {}, fetch, crypto: webcrypto, AbortController, TextEncoder, Uint8Array, Date: FixtureDate, Error, JSON, Array, String, Number, Boolean, Set, Map, encodeURIComponent,
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
  // Task-only observations render independently of a null standalone catalog.
  const scoped = fixture(); await settle(); Object.assign(scoped.config.native_agents[1], {permission_modes: [], reviewer_supported: true});
  scoped.cache = {profiles: [observedEnvelope()]}; await scoped.login();
  const observedContent = scoped.card('review').textContent;
  for (const text of ['尚无缓存', 'Claude 任务上下文观察', '任务 #42', '仓库：task-repo', '审查者（reviewer）', '观察时间：', '2.1.291', '上次读取时任务观察未过期', '任务请求模型：task-requested-alias', '请求 effort：high', '原生模式：claude_restricted', '不是完整实时目录', '不证明账户可调用、获得授权或任务实际生效', '不会用于已验证的目录选择', '手动输入模型 ID（未验证）', 'future-route-from-cli', 'future-effort（supplied dynamically）', '初始化解析模型（非实际生效证明）：resolved-future-route', 'Effort 支持：是', 'Adaptive thinking 支持：是', 'Fast mode 支持：否', 'Auto mode 支持：否', '初始化解析模型（非实际生效证明）：未知', 'Effort 支持：未知（供应商未提供）', '支持的 effort：未知（供应商未提供）', '来源：claude_cli:task_initialize']) assert(observedContent.includes(text), text);
  assert(!scoped.card('dev / one').textContent.includes('Claude 任务上下文观察'));
  assert.equal(scoped.$('capability-profiles').querySelectorAll('select').length, 0);
  assert.equal(scoped.$('capability-profiles').querySelectorAll('input').length, 0);
  assert.equal(scoped.posts().length, 0);
  // Observation freshness is distinct from standalone freshness. Equal-generation
  // updates can carry task evidence, but an old snapshot cannot replace newer evidence.
  const newerObservation = observation({task_id: 43, role: 'developer', checked_at_unix_ms: 1700000020000});
  scoped.cache = {profiles: [observedEnvelope(newerObservation, true, 1, catalog('fresh-standalone'), false)]}; await scoped.$('capability-read').emit('click');
  assert.match(scoped.card('review').textContent, /上次读取时缓存有效/); assert.match(scoped.card('review').textContent, /任务观察已陈旧/); assert.match(scoped.card('review').textContent, /任务 #43/); assert.match(scoped.card('review').textContent, /开发者（developer）/);
  scoped.cache = {profiles: [observedEnvelope(observation(), false, 1, catalog('fresh-standalone'), false)]}; await scoped.$('capability-read').emit('click');
  assert.match(scoped.card('review').textContent, /任务 #43/); assert.match(scoped.card('review').textContent, /任务观察已陈旧/);
  scoped.cache = {profiles: [observedEnvelope(newerObservation, false, 1, catalog('fresh-standalone'), false)]}; await scoped.$('capability-read').emit('click'); assert.match(scoped.card('review').textContent, /任务观察已陈旧/);
  scoped.cache = {profiles: [observedEnvelope(observation({task_id: 44, checked_at_unix_ms: 1700000030000}), false, 1, catalog('fresh-standalone'), true)]}; await scoped.$('capability-read').emit('click');
  assert.match(scoped.card('review').textContent, /缓存已陈旧/); assert.match(scoped.card('review').textContent, /上次读取时任务观察未过期/); assert.match(scoped.card('review').textContent, /任务 #44/);
  scoped.cache = {profiles: [envelope('review', 1, catalog('fresh-standalone'))]}; await scoped.$('capability-read').emit('click'); assert.match(scoped.card('review').textContent, /任务 #44/);
  // Every externally supplied display field is text, even in the observation-only path.
  const attack = '<img src=x onerror=alert(1)>', unsafeModel = {...model(attack, [{effort: attack, description: attack}]), display_name: attack, description: attack, default_effort: attack, resolved_model: attack, source: attack, supports_effort: attack};
  const unsafeObservation = observation({repository: attack, cli_version: attack, requested_model: attack, requested_effort: attack, native_permission: attack, models: [unsafeModel]});
  scoped.cache = {profiles: [observedEnvelope(unsafeObservation, false, 2)]}; await scoped.$('capability-read').emit('click');
  assert(scoped.card('review').textContent.includes('仓库：' + attack)); assert(scoped.card('review').textContent.includes('CLI 版本：' + attack)); assert(scoped.card('review').textContent.includes('初始化解析模型（非实际生效证明）：' + attack)); assert.match(scoped.card('review').textContent, /Effort 支持：未知/); assert.equal(scoped.card('review').querySelectorAll('img').length, 0);
  assert.equal(scoped.card('review').querySelectorAll('script').length, 0);
  // Task observations cannot populate the verified selector, even with a valid
  // epoch/generation; explicit manual entry remains available and unverified.
  scoped.cache = {profiles: [{...observedEnvelope(), cache_epoch: 'a'.repeat(32), generation: 3, stale: false}]}; await scoped.$('capability-read').emit('click');
  scoped.$('agent').value = 'review'; await scoped.$('agent').emit('change'); scoped.$('developer-model-source').value = 'catalog'; await scoped.$('developer-model-source').emit('change');
  assert(scoped.$('developer-model').disabled); assert.equal(scoped.$('developer-model').children.length, 1); assert(!scoped.$('developer-model').textContent.includes('future-route-from-cli')); assert(scoped.$('developer-effort').disabled);
  scoped.$('developer-model-source').value = 'manual'; await scoped.$('developer-model-source').emit('change'); scoped.$('developer-manual-model').value = 'future-route-from-cli'; await scoped.$('developer-manual-model').emit('input');
  assert(!scoped.$('developer-manual-model').disabled); assert.equal(scoped.$('developer-manual-model').value, 'future-route-from-cli'); assert.match(scoped.$('developer-model-note').textContent, /未验证/); assert(scoped.$('developer-effort').disabled); assert.equal(scoped.posts().length, 0);
  // An invalid observation is rejected without erasing the last valid snapshot.
  scoped.cache = {profiles: [{...observedEnvelope({...observation(), task_id: attack}), cache_epoch: 'a'.repeat(32), generation: 4}]}; await scoped.$('capability-read').emit('click');
  assert.match(scoped.$('capability-error').textContent, /格式不符合预期/); assert.match(scoped.card('review').textContent, /任务 #42/);
  // Profile/binary invalidation clears observations at the next generation;
  // an absent optional freshness field stays explicitly unknown.
  scoped.cache = {profiles: [{...observedEnvelope(null, true, 4), cache_epoch: 'a'.repeat(32)}]}; await scoped.$('capability-read').emit('click'); assert(!scoped.card('review').textContent.includes('Claude 任务上下文观察'));
  const uncertain = {...observedEnvelope(observation({models: []}), false, 4), cache_epoch: 'a'.repeat(32)}; delete uncertain.task_observation_stale;
  scoped.cache = {profiles: [uncertain]}; await scoped.$('capability-read').emit('click'); assert.match(scoped.card('review').textContent, /任务观察新鲜度未知/); assert.match(scoped.card('review').textContent, /本次任务未返回模型条目/);
  await scoped.$('logout').emit('click'); assert.equal(scoped.$('capability-profiles').children.length, 0); scoped.events.get('pagehide')();
  // Host opt-in never starts a process on login, panel open, polling or fresh GET.
  // Each explicit start has a fresh cache read and a native, dismissible consent dialog.
  for (const authMode of ['bearer', 'session', 'hybrid']) {
    const start = fixture(authMode), $ = start.$; await settle();
    start.config.native_agents[1].allow_startup_discovery = true; start.validateStartup = true;
    start.cache = {profiles: [startupEnvelope('login-token', start.now + 90000)]}; await start.login();
    assert.equal(start.posts().length, 0); assert.equal(start.startupStarts, 0); assert(!$('startup-discovery-dialog').open);
    await $('capability-toggle').emit('click'); assert(!$('startup-discovery-dialog').open); assert.equal(start.posts().length, 0);
    assert.match(start.card('review').textContent, /宿主已启用启动发现/);
    assert.match(start.card('review').textContent, /不保证无网络、无推理或无费用/);
    start.cache = {profiles: [startupEnvelope('fresh-click-token', start.now + 90000)]}; start.delayRead = true;
    const beforeReads = start.catalogRequests().length, original = start.button('review'), opening = original.emit('click'); await settle();
    await original.emit('click'); await start.button('review').emit('click'); await start.button('dev / one').emit('click');
    assert.equal(start.catalogRequests().length, beforeReads + 1); assert.equal(start.posts().length, 0); assert(!$('startup-discovery-dialog').open);
    start.resolve('/api/capabilities'); await opening; start.delayRead = false;
    assert($('startup-discovery-dialog').open); assert.match($('startup-discovery-title').textContent, /review/);
    assert.equal($('startup-discovery-description').textContent, startupText); assert.equal($('startup-discovery-description').querySelectorAll('img').length, 0);
    assert.match($('startup-discovery-expiry').textContent, /一次刷新/); assert.equal(start.startupStarts, 0);
    await $('startup-discovery-dismiss').emit('click'); await $('startup-discovery-confirm').emit('click');
    assert(!$('startup-discovery-dialog').open); assert.equal(start.posts().length, 0);
    await start.button('review').emit('click'); assert($('startup-discovery-dialog').open);
    await $('startup-discovery-dialog').emit('cancel'); await $('startup-discovery-confirm').emit('click');
    assert(!$('startup-discovery-dialog').open); assert.equal(start.posts().length, 0);
    // Closing the dialog itself invalidates its former confirmation handler.
    await start.button('review').emit('click'); await $('startup-discovery-dialog').close(); await $('startup-discovery-confirm').emit('click'); assert.equal(start.posts().length, 0);
    await start.button('review').emit('click'); start.delayRefresh = true;
    start.refreshResponse = startupEnvelope(null, null, {generation: 2, catalog: {...catalog('confirmed-startup'), provider: 'claude_cli'}, stale: false});
    const confirming = $('startup-discovery-confirm').emit('click'); await settle();
    await $('startup-discovery-confirm').emit('click'); await original.emit('click'); await start.button('review').emit('click');
    assert(!$('startup-discovery-dialog').open); assert.equal(start.posts().length, 1); assert.equal(start.startupStarts, 1);
    assert.deepEqual(JSON.parse(start.posts()[0].opts.body), {confirm_startup_effects: true, confirmation_token: 'fresh-click-token'});
    assert.equal(start.posts()[0].opts.headers['Content-Type'], 'application/json');
    start.resolve('/api/capabilities/review/refresh', 'POST'); await confirming; start.delayRefresh = false;
    assert.match(start.card('review').textContent, /confirmed-startup/);
    // Even a stale/misbehaving response cannot reuse a token already sent once.
    start.cache = {profiles: [startupEnvelope('fresh-click-token', start.now + 90000, {generation: 2})]};
    await start.button('review').emit('click'); assert(!$('startup-discovery-dialog').open); assert.equal(start.posts().length, 1); assert.match(start.card('review').textContent, /已发送的确认不会重用/);
    // A lost response must not trigger retries; even explicit clicks require a new token/dialog.
    start.cache = {profiles: [startupEnvelope('unknown-result-token', start.now + 90000, {generation: 2})]}; await start.button('review').emit('click');
    start.networkRefreshFailure = true; await $('startup-discovery-confirm').emit('click'); start.networkRefreshFailure = false;
    assert.equal(start.posts().length, 2); assert.match(start.card('review').textContent, /不会重放或自动重试/);
    start.cache = {profiles: [startupEnvelope('unknown-result-token', start.now + 90000, {generation: 2})]};
    await start.flushTimer(2000); start.events.get('online')(); await settle(); await start.button('review').emit('click'); await $('startup-discovery-confirm').emit('click');
    assert.equal(start.posts().length, 2); assert(!$('startup-discovery-dialog').open);
    // Logout invalidates consent immediately, including a pending logout response.
    start.cache = {profiles: [startupEnvelope('logout-token', start.now + 90000, {generation: 2})]}; await start.button('review').emit('click');
    start.delayLogout = authMode !== 'bearer'; const logout = $('logout').emit('click'); await settle();
    assert(!$('startup-discovery-dialog').open); await $('startup-discovery-confirm').emit('click'); assert.equal(start.posts().length, 2);
    if (start.delayLogout) start.resolve('/auth/logout', 'POST'); await logout; await start.login();
    assert(!$('startup-discovery-dialog').open); await $('startup-discovery-confirm').emit('click'); assert.equal(start.posts().length, 2);
    start.events.get('pagehide')();
  }
  // Expiry, profile/binary/policy drift and server restart never preserve old consent.
  const consent = fixture(), $c = consent.$; await settle(); consent.config.native_agents[1].allow_startup_discovery = true; consent.validateStartup = true;
  let serial = 0;
  const freshConsent = (extra = {}) => { consent.cache = {profiles: [startupEnvelope('scope-' + (++serial), consent.now + 90000, extra)]}; };
  freshConsent(); await consent.login(); await consent.button('review').emit('click');
  consent.now += 90000; await $c('startup-discovery-confirm').emit('click'); assert.equal(consent.posts().length, 0); assert(!$c('startup-discovery-dialog').open); assert.match(consent.card('review').textContent, /失效/);
  freshConsent(); await consent.button('review').emit('click'); await consent.flushTimer(90000); assert(!$c('startup-discovery-dialog').open); assert.match(consent.card('review').textContent, /已过期/); assert.equal(consent.posts().length, 0);
  freshConsent(); await consent.button('review').emit('click'); freshConsent({generation: 2}); await $c('capability-read').emit('click');
  assert(!$c('startup-discovery-dialog').open); await $c('startup-discovery-confirm').emit('click'); assert.equal(consent.posts().length, 0);
  await consent.button('review').emit('click'); freshConsent({generation: 0, cache_epoch: 'new-process'}); await $c('capability-read').emit('click');
  assert(!$c('startup-discovery-dialog').open); assert.equal(consent.posts().length, 0);
  // Missing configured profile in the fresh snapshot cannot borrow an older cached token.
  await consent.button('review').emit('click'); consent.cache = {profiles: []}; await $c('capability-read').emit('click');
  assert(!$c('startup-discovery-dialog').open); await consent.button('review').emit('click'); assert(!$c('startup-discovery-dialog').open); assert.equal(consent.posts().length, 0);
  // The server rejects unseen drift or token expiry before any startup; UI requires new confirmation.
  for (const reason of ['profile drift', 'executable drift', 'policy drift', 'service epoch drift', 'expired confirmation']) {
    freshConsent({cache_epoch: 'new-process'}); await consent.button('review').emit('click'); assert($c('startup-discovery-dialog').open);
    freshConsent({cache_epoch: 'new-process'}); consent.refreshErrorMessage = reason;
    const before = consent.posts().length; await $c('startup-discovery-confirm').emit('click');
    assert.equal(consent.posts().length, before + 1); assert.equal(consent.startupStarts, 0); assert(!$c('startup-discovery-dialog').open); assert.match(consent.card('review').textContent, /drifted/);
    await consent.flushTimer(2000); assert.equal(consent.posts().length, before + 1);
  }
  for (const event of ['popstate', 'pagehide']) {
    freshConsent({cache_epoch: 'new-process'}); await consent.button('review').emit('click'); assert($c('startup-discovery-dialog').open);
    const posts = consent.posts().length; consent.events.get(event)(); await $c('startup-discovery-confirm').emit('click'); assert.equal(consent.posts().length, posts); assert(!$c('startup-discovery-dialog').open);
  }
  // Authentication failure during the fresh GET or POST discards all pending intent.
  for (const phase of ['GET', 'POST']) {
    const auth = fixture('session'); await settle(); auth.config.native_agents[1].allow_startup_discovery = true; auth.validateStartup = true;
    auth.cache = {profiles: [startupEnvelope('auth-token', auth.now + 90000)]}; await auth.login();
    if (phase === 'GET') auth.readFailure = 401;
    await auth.button('review').emit('click');
    if (phase === 'POST') { auth.refreshFailure = 401; await auth.$('startup-discovery-confirm').emit('click'); }
    assert(!auth.$('startup-discovery-dialog').open); assert(auth.$('capability-panel').hidden); assert(!auth.$('auth-panel').hidden);
    const posts = auth.posts().length; await auth.$('startup-discovery-confirm').emit('click'); assert.equal(auth.posts().length, posts); assert.equal(auth.startupStarts, 0); auth.events.get('pagehide')();
  }
  // Navigation while obtaining the fresh scope fences even an ignored abort response.
  const waiting = fixture(); await settle(); waiting.config.native_agents[1].allow_startup_discovery = true;
  waiting.cache = {profiles: [startupEnvelope('late-token', waiting.now + 90000)]}; await waiting.login(); waiting.delayRead = true; waiting.ignoreAbort = true;
  const late = waiting.button('review').emit('click'); await settle(); waiting.events.get('popstate')(); waiting.resolve('/api/capabilities'); await late;
  assert(!waiting.$('startup-discovery-dialog').open); assert.equal(waiting.posts().length, 0); waiting.events.get('pagehide')();
  assert.match(html, /能力与模型目录不代表账户已登录、可调用模型或获得调用授权/);
  console.log('PASS: confirmed Claude startup fresh-GET/native-dialog/Cancel/Esc/close/expiry/drift/auth/navigation fences; bearer/session/hybrid one-shot POST; no replay or automatic retry after unknown network result; inert confirmation text; capability UI cached-only login/open; explicit bounded-profile refresh; no polling discovery; process epoch/generation/session/navigation fences; restart unlock and stale-finally guard; repeated click guard; safe evidence/models/efforts; task-only scoped observations and optional metadata; independent freshness and same-generation ordering; no observation-backed selector; manual fallback; XSS-safe fields; unknown auth and effective selection; startup context; last-read freshness; local errors; legacy config; bearer/session/hybrid restoration');
})().catch(error => { console.error(error); process.exitCode = 1; });
