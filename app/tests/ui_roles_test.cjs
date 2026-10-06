// Run with node app/tests/ui_roles_test.cjs; executes the production inline UI.
const assert = require('node:assert/strict'), fs = require('node:fs'), vm = require('node:vm');
const {webcrypto} = require('node:crypto');
const html = fs.readFileSync(require('node:path').resolve(__dirname, '../static/index.html'), 'utf8');
const clone = value => JSON.parse(JSON.stringify(value)), settle = () => new Promise(resolve => setImmediate(resolve));
const permission = (id, allowed = true, extra = {}) => ({id, label: id, host_allowed: allowed, availability: allowed ? 'unknown' : 'unsupported', reason: 'native availability is unknown <img src=x>', reviewer_only: id === 'claude_restricted', requires_confirmation: !['codex_workspace_write','claude_restricted'].includes(id), confirmation_text: null, ...extra});
const profile = (name, provider = 'claude_cli') => ({name,provider,model:'configured-model',effort:null,native_permission:null,reviewer_supported:provider === 'claude_cli',permission_modes: provider === 'claude_cli' ? [permission('claude_dont_ask'),permission('claude_auto'),permission('claude_bypass_permissions',false),permission('claude_restricted')] : [permission('codex_workspace_write'),permission('codex_full_access')]});
const config = () => ({repositories:['repo','other'],agents:['generic','codex','claude','other-review'],tests:['test'],native_agents:[profile('codex','codex_app_server'),profile('claude'),profile('other-review')],workflows:[{name:'reviewed',repository:'repo',developer:'codex',reviewer:'claude',test:'test',max_repairs:1,selectable_developers:['codex','claude','generic'],selectable_reviewers:['claude','other-review','codex']},{name:'pinned',repository:'other',developer:'generic',reviewer:'claude',test:'test',max_repairs:0}]});
const envelope = (name, generation = 1, epoch = 'a'.repeat(32), stale = false) => ({name,cache_epoch:epoch,generation,stale,refreshing:false,catalog:{model_catalog:{state:'supported',reason:'catalog verified'},models:[{id:'a',model:'alpha',display_name:'<img src=x onerror=alert(1)>',supported_efforts:[{effort:'low'},{effort:'high'}]},{id:'b',model:'beta',supported_efforts:[{effort:'medium'}]},{id:'c',model:'unknown',supported_efforts:null}]}});
const nativeReviewMode = 'codex_native_sandboxed_review';
const nativeReviewLabel = 'Codex 原生审查（本地只读，允许命令）';
const nativeReviewWarning = '我确认本次审查允许在 Codex 本地只读沙箱中执行命令，原生审批设为 never。原生 hooks、MCP、插件和远程工具不受此本地沙箱约束，仅可使用我信任的启动配置和集成。此模式不等同于严格无执行审查。';
function addNativeReview(f) {
 const p={...profile('codex-review','codex_app_server'),native_permission:nativeReviewMode,reviewer_supported:true,reviewer_contract:'native_local_read_only'};
 p.permission_modes.push(permission(nativeReviewMode,true,{label:nativeReviewLabel,reviewer_only:true,requires_confirmation:true}));
 f.config.agents.push(p.name);f.config.native_agents.push(p);f.config.workflows[0].selectable_reviewers.push(p.name);f.config.workflows[0].selectable_developers.push(p.name);f.cache.profiles.push(envelope(p.name));return p;
}
const estimate = () => ({host_policy_cap_bytes:1000,default_quota_bytes:1000,snapshot_cap_bytes:500,initial_estimate:{source:'host_inventory',snapshot_bytes:100,git_metadata_reference_bytes:10,reviewer_copy_bytes:100,estimated_initial_bytes:210,complete:true,notes:[]}});
function fixture(authMode = 'bearer') {
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
  const f = {nodes,events,timers,requests:[],posts:[],challenges:[],pending:[],timeOffset:0,config:config(),tasks:[],cache:{profiles:['codex','claude','other-review'].map(name=>envelope(name))},mode:'success',delay:()=>false,ignoreAbort:false};
  const $ = id => nodes.get(id); f.$ = $;
  f.login = async () => { $('token').value='token'; $('username').value='operator'; $('password').value='password'; await $('auth-form').emit('submit'); await settle(); };
  f.change = async (id,value) => { $(id).value=value; await $(id).emit('change'); };
  f.choose = async (role,model,effort) => { await f.change(role+'-model-source','catalog'); await f.change(role+'-model',model); if(effort) await f.change(role+'-effort',effort); };
  f.submit = async () => { $('requirements').value='role fixture'; await $('task-form').emit('submit'); await settle(); };
  f.confirm = async (role='developer') => { await $(role+'-challenge').emit('click'); assert(!$(role+'-confirm').disabled, $(role+'-challenge-status').textContent); $(role+'-confirm').checked=true; await $(role+'-confirm').emit('change'); };
  f.resolve = url => { const i=f.pending.findIndex(p=>p.url===url); assert(i>=0,url); f.pending.splice(i,1)[0].resolve(); };
  async function fetch(url,opts) {
    f.requests.push({url,opts}); let data,status=200;
    if(url==='/auth/status')data={mode:authMode,authenticated:false};
    else if(url==='/auth/login')data={authenticated:true};
    else if(url==='/auth/logout')data={authenticated:false};
    else if(url==='/api/config')data=f.config;
    else if(url==='/api/status')data={active:null,recovery_required:false,diagnostic:null};
    else if(url==='/api/tasks'&&opts.method==='GET')data=f.tasks;
    else if(url==='/api/capabilities')data=f.cache;
    else if(url.startsWith('/api/capabilities/')&&url.endsWith('/refresh'))data=f.cache.profiles.find(p=>p.name===decodeURIComponent(url.split('/')[3]));
    else if(url.startsWith('/api/resources?'))data=estimate();
    else if(url==='/api/permission-challenge') {
      const body=JSON.parse(opts.body);f.challenges.push(body);assert.equal(opts.method,'POST');assert(!Object.hasOwn(body.job,'role_binding'));
      for(const role of Object.values(body.job.role_selections||{}))assert(!Object.hasOwn(role,'confirm_permission_expansion'));
      const developer=body.job.role_selections?.developer, profile=f.config.native_agents.find(p=>p.name===body.job.agent);
      data={challenge:'challenge-'+f.challenges.length,expires_at_unix_ms:Date.now()+f.timeOffset+300000,confirmation_text:'Host-resolved scope <img src=x>',scope:{repository:body.job.repository,workflow:body.job.workflow??null,developer:{profile:body.job.agent,provider:profile.provider,model:developer?.model?.value??profile.model,effort:developer?.effort??profile.effort,native_permission:developer?.native_permission??profile.native_permission},reviewer:null}};
      const workflow=f.config.workflows.find(w=>w.name===body.job.workflow),reviewChoice=body.job.role_selections?.reviewer;
      const reviewProfile=f.config.native_agents.find(p=>p.name===(reviewChoice?.profile??workflow?.reviewer));
      if(reviewProfile)data.scope.reviewer={profile:reviewProfile.name,provider:reviewProfile.provider,model:reviewChoice?.model?.value??reviewProfile.model,effort:reviewChoice?.effort??reviewProfile.effort,native_permission:reviewChoice?.native_permission??reviewProfile.native_permission};
      if(f.challengePatch)f.challengePatch(data);
    }
    else if(url==='/api/tasks'&&opts.method==='POST') {
      const body=JSON.parse(opts.body); f.posts.push(body);
      if(f.mode==='abort')throw new TypeError('response lost');
      if(f.mode==='401'||f.mode==='422'){status=Number(f.mode);data={error:'changed host policy'};}
      else {data={id:f.tasks.length+1,key:body.key,payload:JSON.stringify(body.job),state:'queued',generation:0,result:null}; f.tasks.unshift(data);}
    } else if(url.endsWith('/operator')){status=404;data={error:'fixture has no recovery actions'};}
    else if(/^\/api\/tasks\/\d+$/.test(url))data=f.tasks.find(t=>t.id===Number(url.split('/').at(-1)));
    else throw Error(url);
    const snapshot=clone(data),response={ok:status===200,status,json:async()=>clone(snapshot)};
    if(f.delay(url,opts))return new Promise((resolve,reject)=>{f.pending.push({url,signal:opts.signal,resolve:()=>resolve(response)});if(!f.ignoreAbort)opts.signal.addEventListener('abort',()=>{const error=Error('abort');error.name='AbortError';reject(error)});});
    return response;
  }
  class FixtureDate extends Date { constructor(...args) { super(...(args.length ? args : [Date.now()+f.timeOffset])); } static now() { return Date.now()+f.timeOffset; } }
  const context={console,document,window:{matchMedia:()=>({matches:false,addEventListener(){}}),addEventListener:(name,fn)=>events.set(name,fn)},navigator:{},fetch,crypto:webcrypto,AbortController,TextEncoder,Uint8Array,Date:FixtureDate,Error,JSON,Array,String,Number,Boolean,Set,Map,encodeURIComponent,setTimeout:(callback,milliseconds)=>{const id=++timerId;timers.set(id,{callback,milliseconds});return id},clearTimeout:id=>timers.delete(id)};
  Object.defineProperties(context,{localStorage:{get(){throw Error('storage forbidden')}},sessionStorage:{get(){throw Error('storage forbidden')}}});
  vm.runInNewContext(html.match(/<script>([\s\S]*?)<\/script>/)[1],context); return f;
}
(async()=>{
 for(const authMode of ['bearer','session','hybrid']) {
  const f=fixture(authMode),$=f.$;await settle();await f.login();
  assert($('developer-settings').hidden); assert($('reviewer-settings').hidden);
  await f.submit();assert(!Object.hasOwn(f.posts.at(-1).job,'role_selections'));
  await f.change('workflow','reviewed');assert(!$('agent').disabled);assert(!$('reviewer').disabled);
  assert.equal($('reviewer').children.find(o=>o.value==='codex').disabled,true);
  await f.change('agent','claude');await f.choose('developer','alpha','high');await f.choose('reviewer','beta','medium');
  assert.equal($('developer-model').value,'alpha');assert.equal($('reviewer-model').value,'beta');
  assert.equal($('developer-model').querySelectorAll('img').length,0);
  await f.submit();let roles=f.posts.at(-1).job.role_selections;
  assert.equal(f.posts.at(-1).job.agent,'claude');assert.equal(roles.developer.profile,'claude');assert.equal(roles.reviewer.profile,'claude');
  assert.deepEqual(roles.developer.model,{value:'alpha',source:'catalog',catalog:{cache_epoch:'a'.repeat(32),generation:1}});assert.equal(roles.developer.effort,'high');assert.equal(roles.reviewer.effort,'medium');
  // Default workflow still omits all role overrides.
  await f.submit();assert(!Object.hasOwn(f.posts.at(-1).job,'role_selections'));
  // Explicit risk confirmation spells out filesystem, network, and approval effects.
  await f.change('developer-permission','codex_full_access');assert(!$('developer-confirm-field').hidden);
  for(const consequence of ['filesystem AND network','no native approval prompts'])assert($('developer-confirm-text').textContent.includes(consequence));
  const before=f.posts.length;await f.submit();assert.equal(f.posts.length,before);assert.match($('form-message').textContent,/风险确认/);
  await f.confirm();await f.submit();assert.equal(f.posts.at(-1).job.role_selections.developer.confirm_permission_expansion,true);assert.match(f.posts.at(-1).permission_challenge,/^challenge-/);assert.equal(f.challenges.at(-1).job.role_selections.developer.native_permission,'codex_full_access');
  // Switching profile drops incompatible mode, model, effort, confirmation.
  await f.choose('developer','alpha','high');await f.change('developer-permission','codex_full_access');await f.confirm();await f.change('agent','claude');
  assert.equal($('developer-model-source').value,'');assert.equal($('developer-effort').value,'');assert.equal($('developer-permission').value,'');assert(!$('developer-confirm').checked);
  await f.change('developer-permission','claude_auto');assert.match($('developer-permission-note').textContent,/不是 bypass/);assert.match($('developer-permission-note').textContent,/拒绝或回退/);
  await f.change('developer-permission','claude_bypass_permissions');await f.submit();assert.equal(f.posts.length,before+1);assert.match($('form-message').textContent,/原生模式/);
  // Reviewer cannot choose developer modes or Codex; never substitute a different profile.
  await f.change('developer-permission','');await f.change('reviewer-permission','claude_auto');await f.submit();assert.match($('form-message').textContent,/只读角色/);
  await f.change('reviewer-permission','');await f.change('reviewer','codex');await f.submit();assert.match($('form-message').textContent,/技术证明/);assert.equal($('reviewer').value,'codex');
  await f.change('reviewer','claude');assert.match($('reviewer-support').textContent,/不是原生/);
  // Manual fallback cannot invent efforts or carry a catalog reference.
  await f.choose('developer','alpha','high');await f.change('developer-model-source','manual');$('developer-manual-model').value='manual-unverified';await $('developer-manual-model').emit('input');
  assert($('developer-effort').disabled);assert.equal($('developer-effort').value,'');assert.match($('developer-model-note').textContent,/未验证/);
  await f.submit();roles=f.posts.at(-1).job.role_selections;assert.deepEqual(roles.developer.model,{value:'manual-unverified',source:'manual'});assert(!Object.hasOwn(roles.developer,'effort'));
  // Unknown effort metadata offers no invented values; refreshed generations invalidate choices.
  await f.choose('developer','unknown');assert($('developer-effort').disabled);
  await f.choose('developer','alpha','high');f.cache.profiles=f.cache.profiles.map(p=>({...p,generation:2}));await $('capability-read').emit('click');assert.equal($('developer-model').value,'');assert.equal($('developer-effort').value,'');
  const staleCount=f.posts.length;await f.submit();assert.equal(f.posts.length,staleCount);
  await f.choose('developer','alpha','low');f.cache.profiles=f.cache.profiles.map(p=>({...p,stale:true}));await $('capability-read').emit('click');assert($('developer-model').disabled);assert.equal($('developer-model').value,'');
  f.cache.profiles=['codex','claude','other-review'].map(name=>envelope(name,1,'b'.repeat(32)));await $('capability-read').emit('click');await f.choose('developer','alpha','low');
  // Selected reviewer belongs to estimate identity; a late old estimate cannot unlock quota.
  f.delay=url=>url.startsWith('/api/resources?');f.ignoreAbort=true;const old=$('resource-read').emit('click');await settle();const oldURL=f.pending[0].url;assert(oldURL.includes('reviewer_profile=claude'));
  await f.change('reviewer','other-review');f.resolve(oldURL);await old;assert($('workspace-quota').disabled);
  f.delay=()=>false;await $('resource-read').emit('click');assert(f.requests.at(-1).url.includes('reviewer_profile=other-review'));assert(!$('workspace-quota').disabled);
  $('workspace-quota').value='900';await f.change('workflow','pinned');assert.equal($('workspace-quota').value,'');assert($('agent').disabled);assert($('reviewer').disabled);await $('resource-read').emit('click');assert(!f.requests.at(-1).url.includes('reviewer_profile='));
  // Frozen body survives duplicate clicks, catalog restart, auth expiry and config removal.
  await f.change('workflow','reviewed');await f.choose('developer','alpha','high');await f.choose('reviewer','beta','medium');await f.change('developer-permission','codex_full_access');await f.confirm();
  f.mode='abort';f.delay=(url,opts)=>url==='/api/tasks'&&opts.method==='POST'; // network rejection is immediate
  await f.submit();const frozen=clone(f.posts.at(-1));f.delay=()=>false;f.timeOffset+=301000;
  for(const id of ['agent','reviewer','developer-model-source','reviewer-model','developer-permission','developer-confirm'])assert($(id).disabled,id);
  f.cache.profiles=['codex','claude'].map(name=>envelope(name,1,'c'.repeat(32)));await $('capability-read').emit('click');
  f.mode='401';await f.submit();assert.deepEqual(f.posts.at(-1),frozen);assert(!$('auth-panel').hidden);
  f.config={repositories:[],agents:[],tests:[]};f.mode='success';await f.login();assert.match($('reviewer-model-note').textContent,/原提交已冻结/);await f.submit();assert.deepEqual(f.posts.at(-1),frozen);
  // Explicitly stored evidence separates requested / session / observed and is plain text.
  const t=f.tasks[0];t.state='finished';const selection={requested:{profile:'<img src=x>',provider:'codex_app_server',model:'asked'},session_settings:{model:'session-only',source:'thread-start'},observed:{model:null,reroutes:Array.from({length:10},()=>({from_model:'asked',to_model:'rerouted',reason:'<script>bad</script>',thread_id:'t',turn_id:'u'}))},verification:{model:'unknown'},truncated:true};
  t.result=JSON.stringify({agent:{provider:{selection}},workflow:{rounds:[{round:0,developer:{outcome:'success',exit_code:0,summary:'developer StageSummary',selection},reviewer:{outcome:'failure',exit_code:1,summary:'reviewer StageSummary',selection:{requested:{model:'reviewer-request'}}}}]}});
  await $('refresh').emit('click');assert(!$('detail-selection').hidden);const text=$('selection-stages').textContent;
  for(const expected of ['请求：','服务端解析的会话设置','不是每回合证明','实际消息 / reroute 观测：模型 未知','reviewer-request','证据已截断','<script>bad</script>'])assert(text.includes(expected),expected);
  assert.equal($('selection-stages').querySelectorAll('script').length,0);assert.equal($('selection-stages').children.length,5);
  // Cache-page restoration / logout remove role choices and auth, with no model calls.
  f.events.get('pagehide')();f.events.get('pageshow')({persisted:true});await settle();assert($('developer-settings').hidden);assert($('reviewer-settings').hidden);assert.equal($('developer-manual-model').value,'');assert.equal($('reviewer-model').value,'');assert(!$('developer-confirm').checked);
  await $('logout').emit('click');assert($('detail-selection').hidden);assert.equal($('selection-stages').children.length,0);
 }
 // Consent belongs to the exact role selection, including model provenance and effort.
 for(const [id,value] of [['developer-model-source','manual'],['developer-model','beta'],['developer-effort','low'],['repository','other']]) {
   const consent=fixture();await settle();await consent.login();await consent.change('agent','codex');await consent.choose('developer','alpha','high');await consent.change('developer-permission','codex_full_access');await consent.confirm();
   await consent.change(id,value);assert(!consent.$('developer-confirm').checked,id);assert(consent.$('developer-confirm').disabled);await consent.submit();assert.equal(consent.posts.length,0,id);
 }
 const consent=fixture();await settle();await consent.login();await consent.change('agent','codex');await consent.change('developer-permission','codex_full_access');await consent.change('developer-model-source','manual');consent.$('developer-manual-model').value='old-manual';await consent.$('developer-manual-model').emit('input');await consent.confirm();
 consent.$('developer-manual-model').value='manual-new';await consent.$('developer-manual-model').emit('input');assert(!consent.$('developer-confirm').checked);await consent.submit();assert.equal(consent.posts.length,0);
 await consent.choose('developer','alpha','high');await consent.confirm();consent.cache.profiles=consent.cache.profiles.map(p=>({...p,generation:2}));await consent.$('capability-read').emit('click');assert(!consent.$('developer-confirm').checked);
 // Challenge creation is explicit, one in flight, and stale responses never authorize a changed scope.
 const challenge=fixture();await settle();await challenge.login();await challenge.change('agent','codex');await challenge.change('developer-permission','codex_full_access');assert.equal(challenge.challenges.length,0);assert(challenge.$('developer-confirm').disabled);
 challenge.delay=url=>url==='/api/permission-challenge';challenge.ignoreAbort=true;const read=challenge.$('developer-challenge').emit('click');await settle();await challenge.$('developer-challenge').emit('click');assert.equal(challenge.challenges.length,1);await challenge.change('developer-model-source','manual');challenge.resolve('/api/permission-challenge');await read;assert(challenge.$('developer-confirm').disabled);assert.equal(challenge.$('developer-challenge-scope').textContent,'');
 challenge.delay=()=>false;challenge.$('developer-manual-model').value='manual';await challenge.$('developer-manual-model').emit('input');await challenge.confirm();assert.match(challenge.$('developer-challenge-scope').textContent,/model=manual/);assert.equal(challenge.$('developer-confirm-text').querySelectorAll('img').length,0);
 const challengeCount=challenge.challenges.length;challenge.$('requirements').value='Changed text does not change role scope';await challenge.$('requirements').emit('input');assert(challenge.$('developer-confirm').checked);assert.equal(challenge.challenges.length,challengeCount);
 challenge.timeOffset+=301000;await challenge.$('refresh').emit('click');assert(!challenge.$('developer-confirm').checked);assert(challenge.$('developer-confirm').disabled);assert.match(challenge.$('developer-challenge-status').textContent,/已过期/);await challenge.submit();assert.equal(challenge.posts.length,0);
 await challenge.confirm();await challenge.submit();assert.match(challenge.posts[0].permission_challenge,/^challenge-/);
 // Logout while scope resolution is in flight fences the reply and clears all scope text.
 await challenge.change('agent','codex');await challenge.change('developer-permission','codex_full_access');challenge.delay=url=>url==='/api/permission-challenge';const logoutRead=challenge.$('developer-challenge').emit('click');await settle();await challenge.$('logout').emit('click');challenge.resolve('/api/permission-challenge');await logoutRead;assert(challenge.$('developer-settings').hidden);assert.equal(challenge.$('developer-challenge-scope').textContent,'');
 // Native full-access profile default also requires explicit risk confirmation.
 const inherited=fixture();await settle();inherited.config.native_agents[0].native_permission='codex_full_access';await inherited.login();await inherited.change('agent','codex');assert(!inherited.$('developer-confirm-field').hidden);await inherited.submit();assert.equal(inherited.posts.length,0);await inherited.confirm();await inherited.submit();assert.deepEqual(inherited.posts[0].job.role_selections.developer,{profile:'codex',native_permission:'codex_full_access',confirm_permission_expansion:true});
 // Host permission alone never makes an explicitly unsupported native mode usable.
 const unavailable=fixture();unavailable.config.native_agents[1].permission_modes[1].availability='unsupported';unavailable.config.native_agents[1].permission_modes[1].reason='Account eligibility unavailable';await settle();await unavailable.login();await unavailable.change('agent','claude');assert(unavailable.$('developer-permission').children.find(o=>o.value==='claude_auto').disabled);assert.match(unavailable.$('developer-mode-reasons').textContent,/宿主允许；原生不可用/);await unavailable.change('developer-permission','claude_auto');await unavailable.submit();assert.equal(unavailable.posts.length,0);
 // The host's opt-in Codex tier is explicitly distinct from strict no-execution review.
 const native=fixture();addNativeReview(native);await settle();await native.login();await native.change('workflow','reviewed');await native.change('reviewer','codex-review');
 assert(!native.$('reviewer').children.find(o=>o.value==='codex-review').disabled);
 assert.match(native.$('reviewer-support').textContent,/独立 checkout.*全新独立会话.*不恢复旧会话/);
 assert(native.$('reviewer-confirm-text').textContent.includes(nativeReviewWarning));
 assert.match(native.$('reviewer-permission-note').textContent,/不等同于严格无执行审查/);
 assert(native.$('reviewer-permission').children.find(o=>o.value==='codex_full_access').disabled);
 assert(native.$('reviewer-permission').children.find(o=>o.value===nativeReviewMode).textContent.includes(nativeReviewLabel));
 await native.submit();assert.equal(native.posts.length,0);assert.match(native.$('form-message').textContent,/风险确认/);
 await native.confirm('reviewer');assert.match(native.$('reviewer-challenge-scope').textContent,/profile=codex-review.*native_permission=codex_native_sandboxed_review/);
 await native.submit();assert.deepEqual(native.posts[0].job.role_selections.reviewer,{profile:'codex-review',native_permission:nativeReviewMode,confirm_permission_expansion:true});assert(!native.posts[0].job.role_selections.developer);assert.match(native.posts[0].permission_challenge,/^challenge-/);
 // A native-review default never authorizes the developer role, even without an override.
 await native.change('agent','codex-review');assert(native.$('developer-permission').children.find(o=>o.value===nativeReviewMode).disabled);await native.submit();assert.equal(native.posts.length,1);assert.match(native.$('form-message').textContent,/原生模式/);
 await native.change('developer-permission','codex_workspace_write');await native.submit();assert.equal(native.posts.length,2);assert.equal(native.posts[1].job.role_selections.developer.native_permission,'codex_workspace_write');
 const pinnedNative=fixture();addNativeReview(pinnedNative);pinnedNative.config.workflows[0].reviewer='codex-review';await settle();await pinnedNative.login();await pinnedNative.change('workflow','reviewed');await pinnedNative.submit();assert.equal(pinnedNative.posts.length,0);await pinnedNative.confirm('reviewer');await pinnedNative.submit();assert.equal(pinnedNative.posts[0].job.role_selections.reviewer.native_permission,nativeReviewMode);assert(pinnedNative.posts[0].job.role_selections.reviewer.confirm_permission_expansion);
 // Every role/model/effort change revokes native reviewer consent; unchanged old strict configs work above.
 for(const change of [f=>f.change('reviewer','claude'),f=>f.change('reviewer-model','beta'),f=>f.change('reviewer-effort','low'),f=>f.change('reviewer-model-source','manual'),f=>f.change('agent','claude'),f=>f.change('developer-model-source','manual'),async f=>{f.cache.profiles=f.cache.profiles.map(p=>({...p,generation:2}));await f.$('capability-read').emit('click')}]) {
  const f=fixture();addNativeReview(f);await settle();await f.login();await f.change('workflow','reviewed');await f.change('reviewer','codex-review');await f.choose('reviewer','alpha','high');await f.confirm('reviewer');await change(f);assert(!f.$('reviewer-confirm').checked);assert(f.$('reviewer-confirm').disabled);
 }
 // The host enablement, default contract, adapter and confirmation metadata must all agree.
 for(const mutate of [p=>p.reviewer_supported=false,p=>p.reviewer_contract='unsupported',p=>delete p.reviewer_contract,p=>p.provider='codex_cli',p=>p.provider='claude_cli',p=>p.native_permission='codex_workspace_write',p=>p.permission_modes.at(-1).host_allowed=false,p=>p.permission_modes.at(-1).availability='unsupported',p=>p.permission_modes.at(-1).reviewer_only=false,p=>p.permission_modes.at(-1).requires_confirmation=false]) {
  const f=fixture();mutate(addNativeReview(f));await settle();await f.login();await f.change('workflow','reviewed');assert(f.$('reviewer').children.find(o=>o.value==='codex-review').disabled);await f.change('reviewer','codex-review');await f.submit();assert.equal(f.posts.length,0);assert.equal(f.$('reviewer').value,'codex-review');
 }
 // A developer-only or mismatched host scope cannot unlock this reviewer's acknowledgement.
 for(const patch of [v=>v.scope.reviewer=null,v=>v.scope.reviewer.profile='codex',v=>v.scope.reviewer.provider='codex_cli',v=>v.scope.reviewer.native_permission='codex_full_access',v=>v.scope.reviewer.model='different',v=>v.scope.reviewer.effort='high',v=>v.scope.repository='other',v=>v.scope.workflow=null]) {
  const f=fixture();addNativeReview(f);f.challengePatch=patch;await settle();await f.login();await f.change('workflow','reviewed');await f.change('reviewer','codex-review');await f.$('reviewer-challenge').emit('click');assert(f.$('reviewer-confirm').disabled);await f.submit();assert.equal(f.posts.length,0);
 }
 // Review consent has the same stale-response, expiry, and frozen-unknown replay guarantees.
 const reviewReplay=fixture();addNativeReview(reviewReplay);await settle();await reviewReplay.login();await reviewReplay.change('workflow','reviewed');await reviewReplay.change('reviewer','codex-review');
 reviewReplay.delay=url=>url==='/api/permission-challenge';reviewReplay.ignoreAbort=true;const delayedReview=reviewReplay.$('reviewer-challenge').emit('click');await settle();await reviewReplay.change('reviewer-model-source','manual');reviewReplay.resolve('/api/permission-challenge');await delayedReview;assert(reviewReplay.$('reviewer-confirm').disabled);
 reviewReplay.delay=()=>false;await reviewReplay.change('reviewer-model-source','');await reviewReplay.confirm('reviewer');reviewReplay.timeOffset+=301000;await reviewReplay.$('refresh').emit('click');assert(!reviewReplay.$('reviewer-confirm').checked);await reviewReplay.confirm('reviewer');reviewReplay.mode='abort';await reviewReplay.submit();const reviewFrozen=clone(reviewReplay.posts[0]);reviewReplay.timeOffset+=301000;reviewReplay.mode='401';await reviewReplay.submit();assert.deepEqual(reviewReplay.posts.at(-1),reviewFrozen);await reviewReplay.login();reviewReplay.mode='success';await reviewReplay.submit();assert.deepEqual(reviewReplay.posts.at(-1),reviewFrozen);
 // A duplicate in-flight POST is ignored; control changes cannot mutate its request.
 const duplicate=fixture();await settle();await duplicate.login();await duplicate.change('agent','codex');await duplicate.choose('developer','alpha','high');duplicate.delay=(url,opts)=>url==='/api/tasks'&&opts.method==='POST';const post=duplicate.submit();await settle();await duplicate.submit();assert.equal(duplicate.posts.length,1);const frozen=clone(duplicate.posts[0]);await duplicate.change('developer-model','beta');duplicate.resolve('/api/tasks');await post;assert.deepEqual(duplicate.posts[0],frozen);
 // Native Auto-review uses its own explicit scoped consent; it never borrows full-access consent.
 const auto=fixture();auto.config.native_agents[0].provider='codex_app_server';auto.config.native_agents[0].permission_modes.push(permission('codex_auto_review'));await settle();await auto.login();await auto.change('agent','codex');await auto.change('developer-permission','codex_full_access');await auto.confirm();await auto.change('developer-permission','codex_auto_review');assert(!auto.$('developer-confirm').checked);assert(auto.$('developer-confirm').disabled);assert.match(auto.$('developer-permission-note').textContent,/可能自动批准越界请求/);assert.match(auto.$('developer-confirm-text').textContent,/审批模型由 Codex 选择/);await auto.submit();assert.equal(auto.posts.length,0);await auto.confirm();await auto.submit();assert.equal(auto.posts[0].job.role_selections.developer.native_permission,'codex_auto_review');assert.equal(auto.posts[0].job.role_selections.developer.confirm_permission_expansion,true);
 const autoTask=auto.tasks[0];autoTask.state='finished';autoTask.result=JSON.stringify({agent:{provider:{selection:{requested:{native_permission:'codex_auto_review',approvals_reviewer:'auto_review'},session_settings:{approval_policy:'on-request',approvals_reviewer:'auto_review',source:'codex.thread/start'},observed:{native_approval_reviews:[{status:'denied',action_type:'networkAccess',rationale:'<img src=x> native refusal',review_id:'review-1',source:'codex.item/autoApprovalReview/completed'}]},verification:{permission:'session_reported'}}}}});await auto.$('refresh').emit('click');assert.match(auto.$('selection-stages').textContent,/approvals_reviewer auto_review/);assert.match(auto.$('selection-stages').textContent,/denied/);assert.match(auto.$('selection-stages').textContent,/不证明操作已执行/);assert.equal(auto.$('selection-stages').querySelectorAll('img').length,0);
 console.log('PASS: opt-in Codex native review contract/default/host/adapter gates, explicit consent, default-mode role separation, scope mismatch and frozen replay; native role UI independent same-profile roles; pinned/allowed defaults; catalog epoch/generation/staleness; manual unverified fallback; supported effort only; server scope challenges and exact-selection consent; expiry/late-response fencing; unsupported reviewer/modes; reviewer estimate fences; exact frozen replay and duplicate guards; safe requested/session/observed evidence; auth/logout/back-forward');
})().catch(error=>{console.error(error);process.exitCode=1});
