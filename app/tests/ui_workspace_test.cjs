// Run with: node app/tests/ui_workspace_test.cjs
// Actual UI code with deterministic network/clock; no host scan or filesystem cleanup.
const assert = require('node:assert/strict'), fs = require('node:fs'), vm = require('node:vm');
const {webcrypto} = require('node:crypto');
const html = fs.readFileSync(require('node:path').resolve(__dirname, '../static/index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const clone = value => JSON.parse(JSON.stringify(value));
const settle = () => new Promise(resolve => setImmediate(resolve));
const attack = '<img src=x onerror=window.inventoryAttack=1>';
const entry = (id, status = 'disabled') => ({workspace_task_id:id,path:'/fixture/task-'+id,current_owner:{task_id:id,generation:1,owner:'host',state:'finished'},references:[{task_id:id,generation:1,state:'finished',outcome:'success'}],references_complete:true,successor_reserved:false,allocated_usage:{allocated_bytes:4096,complete:true,measured_at:1700000000,reason:null},retention:{status,reason:'fixture retention evidence',eligible_at:status==='eligible'?1699999999:null}});
const page = (entries = [entry(3), entry(2)], next = 2) => ({observed_at:1700000001,policy:{successful_retention_seconds:null,automatic_cleanup_enabled:false},workspaces:entries,next_before:next,complete:true,reason:null});
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
  const f = {nodes, events, timers, requests:[], pending:[], authenticated:restored, delay:false, delayLogout:false, logoutFailure:false, ignoreAbort:false, failure:0, pages:new Map([['/api/workspaces', page()], ['/api/workspaces?before=2', page([entry(1)],null)]])};
  const $ = id => nodes.get(id); f.$ = $;
  f.reads = () => f.requests.filter(r=>r.url.startsWith('/api/workspaces'));
  f.login = async () => { $('token').value='test-token'; $('username').value='operator'; $('password').value='password'; await $('auth-form').emit('submit'); await settle(); };
  f.open = () => $('inventory-toggle').emit('click');
  f.read = () => $('inventory-read').emit('click');
  f.card = id => $('inventory-entries').children.find(node=>node.dataset.workspaceTaskId===String(id));
  f.resolve = url => { const i=f.pending.findIndex(p=>p.url===url); assert(i>=0,'missing '+url); f.pending.splice(i,1)[0].resolve(); };
  f.flush = async ms => { for (const [id,timer] of [...timers]) if(timer.milliseconds===ms){timers.delete(id);timer.callback();} await settle(); };
  async function fetch(url, opts) {
    f.requests.push({url,opts}); let data,status=200,delayed=false;
    assert.equal(opts.headers.Authorization,authMode==='bearer'&&url.startsWith('/api/')?'Bearer test-token':undefined);
    assert.equal(opts.credentials,authMode==='bearer'&&url.startsWith('/api/')?'omit':'same-origin'); assert.equal(opts.redirect,'error'); assert.equal(opts.cache,'no-store');
    if(url==='/auth/status')data={mode:authMode,authenticated:f.authenticated};
    else if(url==='/auth/login'){f.authenticated=true;data={authenticated:true};}
    else if(url==='/auth/logout'){if(f.logoutFailure)throw new TypeError('offline');f.authenticated=false;data={authenticated:false};delayed=f.delayLogout;}
    else if(url==='/api/config')data={repositories:['repo'],agents:['agent'],tests:[]};
    else if(url==='/api/status')data={active:null,recovery_required:false,diagnostic:null};
    else if(url==='/api/tasks')data=[];
    else if(url.startsWith('/api/workspaces')){assert.equal(opts.method,'GET');assert.equal(opts.body,undefined);data=f.pages.get(url);delayed=f.delay;if(f.failure){status=f.failure;data={error:'unavailable '+attack};}}
    else throw new Error('Unexpected request '+url);
    const snapshot=clone(data),response={ok:status===200,status,json:async()=>clone(snapshot)};
    if(delayed)return new Promise((resolve,reject)=>{f.pending.push({url,resolve:()=>resolve(response)});if(!f.ignoreAbort)opts.signal.addEventListener('abort',()=>{const e=new Error('abort');e.name='AbortError';reject(e);});});
    return response;
  }
  const context = {console,document,window:{matchMedia:()=>({matches:false,addEventListener(){}}),addEventListener:(name,fn)=>events.set(name,fn)},navigator:{},fetch,crypto:webcrypto,AbortController,TextEncoder,Uint8Array,Date,Error,JSON,Array,String,Number,Boolean,Set,Map,encodeURIComponent,setTimeout:(callback,milliseconds)=>{const id=++timerId;timers.set(id,{callback,milliseconds});return id;},clearTimeout:id=>timers.delete(id)};
  Object.defineProperties(context,{localStorage:{get(){throw Error('must not use storage')}},sessionStorage:{get(){throw Error('must not use storage')}}});
  vm.runInNewContext(script,context);return f;
}
(async()=>{
  const f=fixture(),$=f.$;await settle();assert($('inventory-panel').hidden);await f.read();assert.equal(f.reads().length,0);
  await f.login();assert(!$('inventory-panel').hidden);assert($('inventory-body').hidden);assert.equal(f.reads().length,0);
  await f.open();assert.equal($('inventory-toggle').getAttribute('aria-expanded'),'true');assert(!$('inventory-body').hidden);assert.equal(f.reads().length,0,'expanding must not scan');
  await $('refresh').emit('click');await f.flush(2000);assert.equal(f.reads().length,0,'queue refresh and polling must not scan');
  await f.read();assert.equal(f.reads().length,1);assert(f.card(3));assert(f.card(2));assert(!$('inventory-next').disabled);assert($('inventory-prev').disabled);
  assert.match($('inventory-policy').textContent,/未配置；自动清理已关闭/);assert.match($('inventory-status').textContent,/快照时间/);assert.match($('inventory-page-status').textContent,/第 1 页/);assert.match(f.card(3).textContent,/4096 字节/);assert.match(f.card(3).textContent,/任务 #3 · 代次 1 · host/);
  await $('inventory-next').emit('click');assert.equal(f.reads().at(-1).url,'/api/workspaces?before=2');assert(f.card(1));assert(!f.card(3));assert($('inventory-next').disabled);assert(!$('inventory-prev').disabled);assert.match($('inventory-page-status').textContent,/第 2 页/);
  await $('inventory-prev').emit('click');assert.equal(f.reads().at(-1).url,'/api/workspaces');assert(f.card(3));assert($('inventory-prev').disabled);
  // Display server-owned policy/status literally; never infer eligibility from outcomes/age.
  const first=page(['eligible','waiting','disabled','protected','unknown'].map((status,i)=>entry(20-i,status)),null);
  first.policy={successful_retention_seconds:3600,automatic_cleanup_enabled:true};first.complete=false;first.reason='Bounded enumeration '+attack;
  const e=first.workspaces[4];e.current_owner=null;e.references_complete=false;e.successor_reserved=true;e.path='/fixture/'+attack+'x'.repeat(300);e.retention.reason=attack;e.allocated_usage={allocated_bytes:null,complete:false,measured_at:1700000000,reason:attack};e.references=[{task_id:42,generation:0,state:attack,outcome:null}];
  first.workspaces[0].allocated_usage.allocated_bytes=0;first.workspaces[1].allocated_usage.complete=false;first.workspaces[1].references=[];
  f.pages.set('/api/workspaces',first);await f.read();assert.match($('inventory-policy').textContent,/3600 秒；自动清理已开启/);assert.match($('inventory-status').textContent,/不完整/);assert.match($('inventory-page-status').textContent,/不代表清单完整/);
  for(const label of ['符合保留期限（快照）','等待保留期限','自动清理关闭','受保护','保留资格未知'])assert($('inventory-entries').textContent.includes(label));
  assert.match(f.card(20).textContent,/已分配字节（块占用）0 B/);assert.match(f.card(19).textContent,/已观测 4.00 KiB/);assert.match(f.card(16).textContent,/已分配字节（块占用）未知/);assert.match(f.card(16).textContent,/未知 \/ 未确认绑定/);assert.match(f.card(16).textContent,/不完整，仍有未知引用/);assert.match(f.card(16).textContent,/结果 未知/);assert.match(f.card(19).textContent,/不代表工作区无人使用/);assert(f.card(16).textContent.includes(attack));assert.equal($('inventory-entries').querySelectorAll('img').length,0);assert.equal($('inventory-entries').querySelectorAll('button').length,0);assert.equal($('inventory-entries').querySelectorAll('a').length,0);
  const reads=f.reads().length;await f.flush(2000);await $('refresh').emit('click');assert.equal(f.reads().length,reads);
  // Errors invalidate the previous policy, sizes, eligibility and page cursor.
  f.failure=503;await f.read();assert.equal($('inventory-entries').children.length,0);assert.equal($('inventory-policy').textContent,'');assert.match($('inventory-status').textContent,/资格未知/);assert($('inventory-next').disabled);assert($('inventory-error').textContent.includes(attack));assert.equal($('inventory-error').children.length,0);f.failure=0;
  // Optional publication deadlines are exact host timestamps, independent of cleanup eligibility.
  f.failure=0;const deadline=page();deadline.workspaces[0].retention.authorization_expires_at=1700003600;deadline.workspaces[1].retention.authorization_expires_at=null;f.pages.set('/api/workspaces',deadline);await f.read();assert.match(f.card(3).textContent,/发布授权固定到期时间/);assert.match(f.card(3).textContent,/Unix 1700003600 秒；重试不延长/);assert(!f.card(2).textContent.includes('发布授权固定到期时间'));
  // Strict bounded contracts: reject unsafe numbers, unknown enum, malformed arrays/cursors.
  const invalid=[p=>p.workspaces[0].retention.authorization_expires_at=-1,p=>p.workspaces[0].retention.authorization_expires_at=Number.MAX_SAFE_INTEGER,p=>p.workspaces[0].retention.authorization_expires_at=attack,p=>p.workspaces[0].allocated_usage.allocated_bytes=-1,p=>p.workspaces[0].allocated_usage.allocated_bytes=Number.MAX_SAFE_INTEGER+1,p=>p.workspaces[0].retention.status='safe_to_delete',p=>p.workspaces[0].references=null,p=>p.workspaces[0].references=Array(101).fill(p.workspaces[0].references[0]),p=>p.workspaces=Array(17).fill(entry(2)),p=>p.next_before=0,p=>p.next_before=1,p=>p.next_before=4,p=>p.workspaces[1].workspace_task_id=3,p=>p.observed_at='1700000000',p=>p.policy.automatic_cleanup_enabled=null,p=>p.workspaces[0].current_owner.owner=null];
  for(const mutate of invalid){const p=page();mutate(p);f.pages.set('/api/workspaces',p);await f.read();assert.match($('inventory-error').textContent,/格式或分页游标/);assert.equal($('inventory-entries').children.length,0);assert($('inventory-next').disabled);}
  f.pages.set('/api/workspaces',page());f.pages.set('/api/workspaces?before=2',page([entry(2)],null));await f.read();await $('inventory-next').emit('click');assert.match($('inventory-error').textContent,/分页游标/);assert.equal($('inventory-entries').children.length,0);f.pages.set('/api/workspaces?before=2',page([entry(1)],null));
  for(const complete of [true,false]){const empty=page([],null);empty.complete=complete;f.pages.set('/api/workspaces',empty);await f.read();assert.match($('inventory-entries').textContent,complete?/未发现工作区/:/清单不完整/);assert($('inventory-next').disabled);}
  // New explicit reads abort old requests and ignore late responses even if abort is ignored.
  f.pages.set('/api/workspaces',page());f.delay=true;f.ignoreAbort=true;const old=f.read();await settle();const oldRequest=f.reads().at(-1);assert.equal($('inventory-entries').getAttribute('aria-busy'),'true');assert($('inventory-next').disabled);
  f.delay=false;f.pages.set('/api/workspaces',page([entry(99)],null));await f.read();assert(oldRequest.opts.signal.aborted);f.resolve('/api/workspaces');await old;assert(f.card(99));assert(!f.card(3));
  f.pages.set('/api/workspaces',page());await f.read();f.delay=true;const oldPage=$('inventory-next').emit('click');await settle();f.delay=false;await f.read();f.resolve('/api/workspaces?before=2');await oldPage;assert(f.card(3));assert(!f.card(1));assert.match($('inventory-page-status').textContent,/第 1 页/);
  // Late failure cannot invalidate a newer successful read.
  f.failure=503;f.delay=true;const oldFailure=f.read();await settle();f.failure=0;f.delay=false;await f.read();f.resolve('/api/workspaces');await oldFailure;assert(f.card(3));assert($('inventory-error').hidden);
  // Closing aborts and drops data; reopening is an explicit read decision again.
  f.delay=true;const closed=f.read();await settle();await f.open();f.resolve('/api/workspaces');await closed;assert($('inventory-body').hidden);assert.equal($('inventory-entries').children.length,0);assert.equal($('inventory-toggle').getAttribute('aria-expanded'),'false');await f.open();assert.equal($('inventory-entries').children.length,0);f.delay=false;f.ignoreAbort=false;
  // Timeout remains unknown and does not automatically retry.
  f.delay=true;const timeout=f.read();await settle();await f.flush(20000);await timeout;assert.match($('inventory-error').textContent,/请求超时/);assert.equal($('inventory-entries').children.length,0);f.pending=[];f.delay=false;
  // Logout fences in-flight successful responses; re-login does not scan or reveal history.
  f.delay=true;f.ignoreAbort=true;const stale=f.read();await settle();await $('logout').emit('click');f.resolve('/api/workspaces');await stale;assert($('inventory-panel').hidden);assert.equal($('inventory-entries').children.length,0);assert.equal($('inventory-policy').textContent,'');f.delay=false;const count=f.reads().length;await f.login();assert.equal(f.reads().length,count);assert($('inventory-body').hidden);
  for(const status of [401,403]){await f.open();f.failure=status;await f.read();assert(!$('auth-panel').hidden);assert($('inventory-panel').hidden);assert.equal($('inventory-entries').children.length,0);f.failure=0;await f.login();}
  await f.open();await f.read();f.events.get('pagehide')();assert.equal($('inventory-entries').children.length,0);f.events.get('pageshow')({persisted:true});assert($('inventory-panel').hidden);
  // Cookie and hybrid auth follow existing same-origin/no-bearer behavior, including failed logout.
  for(const authMode of ['session','hybrid']){const g=fixture(authMode,true),q=g.$;await settle();await settle();assert.equal(g.reads().length,0);await g.open();await g.read();assert(g.card(3));g.logoutFailure=true;await q('logout').emit('click');assert(q('auth-panel').hidden);assert.equal(q('inventory-entries').children.length,0);assert(!q('inventory-toggle').disabled);assert.match(q('logout-error').textContent,/未确认/);g.logoutFailure=false;await g.open();g.delay=true;g.ignoreAbort=true;const late=g.read();await settle();g.delayLogout=true;const logout=q('logout').emit('click');await settle();assert(q('inventory-read').disabled);g.resolve('/api/workspaces');await late;assert.equal(q('inventory-entries').children.length,0);g.resolve('/auth/logout');await logout;assert(q('inventory-panel').hidden);assert.equal(q('inventory-policy').textContent,'');}
  const inventoryMarkup=html.slice(html.indexOf('<section id="inventory-panel"'),html.indexOf('    <div class="workspace">'));
  assert.match(inventoryMarkup,/不是可安全删除的结论或删除授权/);assert.match(inventoryMarkup,/不是逻辑容量、独占占用或可回收空间/);assert.match(inventoryMarkup,/不根据 GitHub 合并状态/);assert.match(inventoryMarkup,/旧配置根或其他路径未扫描/);assert.match(inventoryMarkup,/aria-controls="inventory-body"/);assert.match(inventoryMarkup,/aria-label="工作区分页"/);
  assert(f.reads().every(request=>request.opts.method==='GET'&&!request.opts.body));
  console.log('PASS: workspace UI: explicit read-only bounded pages, policy/allocated-byte/unknown semantics, descending cursor validation, no polling or host actions, safe text, error invalidation, races/abort/timeout, logout/history fencing, bearer/session/hybrid auth');
})().catch(error=>{console.error(error);process.exitCode=1});
