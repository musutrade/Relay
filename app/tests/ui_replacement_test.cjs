// Run with node app/tests/ui_replacement_test.cjs; executes the production inline UI.
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

function addKiro(f, allowed = true) {
 const p={name:'kiro',provider:'kiro_cli',model:null,effort:null,native_permission:null,reviewer_supported:false,reviewer_contract:'unsupported',permission_modes:[permission('kiro_workspace_write',allowed,{label:'Kiro native workspace editing',reason:'edit-workspace only; no OS sandbox; operator-trusted hooks, MCP and settings',reviewer_only:false,requires_confirmation:true})]};
 f.config.agents.push(p.name);f.config.native_agents.push(p);f.config.workflows[0].selectable_developers.push(p.name);f.config.workflows[0].selectable_reviewers.push(p.name);f.cache.profiles.push(envelope(p.name));return p;
}
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
  const f = {nodes,events,timers,requests:[],posts:[],challenges:[],pending:[],timeOffset:0,operators:new Map(),status:{active:null,recovery_required:false,diagnostic:null},winner:null,challengePatch:null,config:config(),tasks:[],cache:{profiles:['codex','claude','other-review'].map(name=>envelope(name))},mode:'success',delay:()=>false,ignoreAbort:false};
  const $ = id => nodes.get(id); f.$ = $;
  f.login = async () => { $('token').value='token'; $('username').value='operator'; $('password').value='password'; await $('auth-form').emit('submit'); await settle(); };
  f.change = async (id,value) => { $(id).value=value; await $(id).emit('change'); };
  f.choose = async (role,model,effort) => { await f.change(role+'-model-source','catalog'); await f.change(role+'-model',model); if(effort) await f.change(role+'-effort',effort); };
  f.submit = async () => { $('requirements').value='role fixture'; await $('task-form').emit('submit'); await settle(); };
  f.confirm = async () => { await $('developer-challenge').emit('click'); assert(!$('developer-confirm').disabled, $('developer-challenge-status').textContent); $('developer-confirm').checked=true; await $('developer-confirm').emit('change'); };
  f.resolve = url => { const i=f.pending.findIndex(p=>p.url===url); assert(i>=0,url); f.pending.splice(i,1)[0].resolve(); };
  async function fetch(url,opts) {
    f.requests.push({url,opts}); let data,status=200;
    if(url==='/auth/status')data={mode:authMode,authenticated:false};
    else if(url==='/auth/login')data={authenticated:true};
    else if(url==='/auth/logout')data={authenticated:false};
    else if(url.endsWith('/merge-preview'))data={eligible:false,reason:'Merge disabled in fixture',policies:[],authorizations:[],risk_disclosure:null,lane_diagnostic:null};
    else if(url.endsWith('/merge-authorizations'))data=[];
    else if (url==='/api/config')data=f.config;
    else if(url==='/api/status')data=f.status;
    else if(url==='/api/tasks'&&opts.method==='GET')data=f.tasks;
    else if(url==='/api/capabilities')data=f.cache;
    else if(url.startsWith('/api/capabilities/')&&url.endsWith('/refresh'))data=f.cache.profiles.find(p=>p.name===decodeURIComponent(url.split('/')[3]));
    else if(url.startsWith('/api/resources?'))data=estimate();
    else if(url.endsWith('/replacement-challenge')) {
      const body=JSON.parse(opts.body),id=Number(url.split('/').at(-2)); f.challenges.push({id,...body});
      assert.equal(opts.method,'POST');assert(!Object.hasOwn(body.replacement,'confirm_permission_expansion'));
      const role=body.action==='continue_review'?'reviewer':'developer',p=f.config.native_agents.find(p=>p.name===body.replacement.profile);
      data={challenge:'replacement-token-'+f.challenges.length,expires_at_unix_ms:Date.now()+f.timeOffset+300000,confirmation_text:'Exact replacement scope <img src=x>',scope:{predecessor_task_id:id,action:body.action,role,repository:'repo',workflow:'reviewed',[role]:{profile:body.replacement.profile,provider:p.provider,model:body.replacement.model?.value??p.model,effort:body.replacement.effort??p.effort,native_permission:body.replacement.native_permission??p.native_permission}}};
      if(f.challengePatch)f.challengePatch(data);
    }
    else if(url.endsWith('/retry')||url.endsWith('/continue-review')) {
      const id=Number(url.split('/').at(-2)),body=JSON.parse(opts.body);f.posts.push({id,path:url,...body});
      if(f.mode==='abort')throw new TypeError('response lost');
      if(['401','422'].includes(f.mode)){status=Number(f.mode);data={error:'expired challenge after restart'};}
      else {
        const accepted=f.winner||{id,path:url,...body},old=f.tasks.find(t=>t.id===id),job=JSON.parse(old.payload),role=accepted.path.endsWith('/continue-review')?'reviewer':'developer';
        const continuation={predecessor_task_id:id,predecessor_generation:1,workspace_task_id:id,...(role==='reviewer'?{review_only:{candidate_sha:'c'.repeat(40),base_sha:'b'.repeat(40),round:1}}:{})};
        if(accepted.replacement){job.role_selections={...job.role_selections,[role]:accepted.replacement};if(role==='developer')job.agent=accepted.replacement.profile;continuation.replacement={role,session_epoch:1};}
        data={id:100+id,key:accepted.key,payload:JSON.stringify({...job,continuation}),state:'queued',generation:0,result:null};f.tasks.unshift(data);old.continuation_status={successor_id:data.id};
      }
    }
    else if(url==='/api/permission-challenge') {
      const body=JSON.parse(opts.body);f.challenges.push(body);assert.equal(opts.method,'POST');assert(!Object.hasOwn(body.job,'role_binding'));
      for(const role of Object.values(body.job.role_selections||{}))assert(!Object.hasOwn(role,'confirm_permission_expansion'));
      const developer=body.job.role_selections?.developer, profile=f.config.native_agents.find(p=>p.name===body.job.agent);
      data={challenge:'challenge-'+f.challenges.length,expires_at_unix_ms:Date.now()+f.timeOffset+300000,confirmation_text:'Host-resolved scope <img src=x>',scope:{repository:body.job.repository,workflow:body.job.workflow??null,developer:{profile:body.job.agent,provider:profile.provider,model:developer?.model?.value??profile.model,effort:developer?.effort??profile.effort,native_permission:developer?.native_permission??profile.native_permission},reviewer:null}};
    }
    else if(url==='/api/tasks'&&opts.method==='POST') {
      const body=JSON.parse(opts.body); f.posts.push(body);
      if(f.mode==='abort')throw new TypeError('response lost');
      if(f.mode==='401'||f.mode==='422'){status=Number(f.mode);data={error:'changed host policy'};}
      else {data={id:f.tasks.length+1,key:body.key,payload:JSON.stringify(body.job),state:'queued',generation:0,result:null}; f.tasks.unshift(data);}
    } else if(url.endsWith('/operator')){const id=Number(url.split('/').at(-2));data=f.operators.get(id);if(!data){status=404;data={error:'fixture has no recovery actions'};}}
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

function stoppedTask(id=20) { return {id,key:'old-'+id,payload:JSON.stringify({repository:'repo',requirements:'retained work',agent:'codex',workflow:'reviewed',test:'test',publish:false}),state:'finished',generation:1,result:JSON.stringify({outcome:'failure',workspace:'/retained/'+id,draft_pr:null})}; }
function operator(id=20,role='developer') {
 const stage={role,round:1,base_sha:'b'.repeat(40),candidate_sha:'c'.repeat(40),max_repairs:2,remaining_repairs:1};
 return {task_id:id,generation:1,failure:{code:'native_failure',stage:role,cause:'stopped <img src=x>'},resources:{usage:{logical_bytes:400,complete:true,measured_at:1700000000,reason:null},quota_bytes:500,host_policy_cap_bytes:1000,snapshot_cap_bytes:500},retained_result:{available:true,immutable:true},workspace_retained:true,recovery:{inherited_quota_bytes:500,blocked_reason:null,successor_id:null,reserved_request:null,actions:[{id:role==='reviewer'?'continue_review':'retry',ordinary_allowed:true,quota_increase_allowed:true,quota_increase_required:false,min_quota_bytes:501,max_quota_bytes:1000,requires_test_revalidation:role==='reviewer',replacement:{allowed:true,role,reason:null,profiles:role==='reviewer'?['claude','other-review','codex']:['codex','claude','generic'],stopped_stage:stage}}]}};
}
async function ready(role='developer',auth='bearer') {const f=fixture(auth);f.tasks=[stoppedTask()];f.operators.set(20,operator(20,role));await settle();await f.login();return f;}
async function open(f,role='developer') {await f.$(role==='reviewer'?'review-task':'retry-task').emit('click');assert(f.$('retry-dialog').open);}
async function replaceWith(f,name='codex') {f.$('retry-replace').checked=true;await f.$('retry-replace').emit('change');await f.change('replacement-profile',name);}
async function select(f,id) {await f.$('task-list').querySelectorAll('button').find(n=>n.dataset.taskId===String(id)).emit('click');}
async function expanded(f) {await replaceWith(f);await f.change('replacement-permission','codex_full_access');}
async function consent(f) {await f.$('replacement-challenge').emit('click');assert(!f.$('replacement-confirm').disabled,f.$('replacement-challenge-status').textContent);f.$('replacement-confirm').checked=true;await f.$('replacement-confirm').emit('change');}
async function choose(f,value='alpha',effort='high') {await f.change('replacement-model-source','catalog');await f.change('replacement-model',value);if(effort)await f.change('replacement-effort',effort);}
async function confirm(f) {await f.$('retry-confirm').emit('click');await settle();}
function saved(body,role='developer') {return {action_id:role==='reviewer'?'continue_review':'retry',key:body.key,workspace_quota_bytes:body.workspace_quota_bytes??null,revalidate_tests:role==='reviewer',review_focus:body.review_focus??null,replacement:body.replacement??null};}
(async()=>{
 // Ordinary continuation is unchanged and never requests expansion for another role.
 const ordinary=await ready();await open(ordinary);assert(!ordinary.$('retry-replace').checked);await confirm(ordinary);assert(!Object.hasOwn(ordinary.posts[0],'replacement'));assert.equal(ordinary.challenges.length,0);
 // Legacy and negative host capability evidence cannot be inferred from matching log text.
 for(const reason of [null,'Missing stopped stage proof','Unknown process still needs reconciliation','Publication attempt requires reconciliation','Codex reviewer isolation is unproven']){
   const f=await ready();const action=f.operators.get(20).recovery.actions[0];if(reason===null)delete action.replacement;else Object.assign(action.replacement,{allowed:false,reason,stopped_stage:null});
   await f.$('operator-read').emit('click');await open(f);assert(f.$('retry-replace').disabled);assert.match(f.$('retry-replacement-status').textContent,reason?new RegExp(reason):/缺少服务端阶段证明/);
   f.$('retry-replace').checked=true;await f.$('retry-replace').emit('change');assert(!f.$('retry-replace').checked);await confirm(f);assert(!f.posts[0].replacement);
 }
 // Replacement-only actions require an explicit opt-in, never silently restart.
 const only=await ready();only.operators.get(20).recovery.actions[0].ordinary_allowed=false;await only.$('operator-read').emit('click');await open(only);await confirm(only);assert.equal(only.posts.length,0);assert.match(only.$('retry-dialog-error').textContent,/显式选择/);await replaceWith(only,'generic');await confirm(only);assert.deepEqual(only.posts[0].replacement,{profile:'generic'});
 // Cached selections and hostile labels stay bounded text; unsupported modes stay disabled.
 const catalog=await ready();await open(catalog);await replaceWith(catalog);await choose(catalog);assert.equal(catalog.$('replacement-model').querySelectorAll('img').length,0);assert.match(catalog.$('replacement-model').textContent,/<img/);assert.match(catalog.$('replacement-session').textContent,/旧原生历史不能跨供应商/);assert.match(catalog.$('replacement-stage').textContent,/原轮次 1.*剩余修复预算 1/);
 await confirm(catalog);assert.deepEqual(catalog.posts[0].replacement,{profile:'codex',model:{value:'alpha',source:'catalog',catalog:{cache_epoch:'a'.repeat(32),generation:1}},effort:'high'});assert.match(catalog.$('continuation-choice').textContent,/配置 codex/);
 const manual=await ready();await open(manual);await replaceWith(manual);await choose(manual);await manual.change('replacement-model-source','manual');manual.$('replacement-manual-model').value='manual-unverified';await manual.$('replacement-manual-model').emit('input');assert(manual.$('replacement-effort').disabled);assert.equal(manual.$('replacement-effort').value,'');assert.match(manual.$('replacement-model-note').textContent,/未验证/);await confirm(manual);assert.deepEqual(manual.posts[0].replacement.model,{value:'manual-unverified',source:'manual'});assert(!Object.hasOwn(manual.posts[0].replacement,'effort'));
 // Reviewer-only replacement retains exact candidate, test once, no development or repair.
 const reviewer=await ready('reviewer');await open(reviewer,'reviewer');await replaceWith(reviewer,'other-review');assert(reviewer.$('replacement-profile').children.find(o=>o.value==='codex').disabled);assert.match(reviewer.$('replacement-stage').textContent,/cccccccc.*只运行一次.*不开发、不修复/);assert.match(reviewer.$('retry-dialog-description').textContent,/不调用开发者/);await reviewer.change('replacement-permission','claude_restricted');await confirm(reviewer);assert.equal(reviewer.posts[0].revalidate_tests,true);assert.equal(reviewer.posts[0].replacement.profile,'other-review');assert.equal(reviewer.challenges.length,0);
 const codexReviewer=await ready('reviewer');await open(codexReviewer,'reviewer');await replaceWith(codexReviewer,'codex');await confirm(codexReviewer);assert.equal(codexReviewer.posts.length,0);assert.match(codexReviewer.$('retry-dialog-error').textContent,/隔离技术证明/);
 const unavailable=await ready();await open(unavailable);await replaceWith(unavailable,'claude');assert(unavailable.$('replacement-permission').children.find(o=>o.value==='claude_bypass_permissions').disabled);await unavailable.change('replacement-permission','claude_bypass_permissions');await confirm(unavailable);assert.equal(unavailable.posts.length,0);
 // Explicit native review replacement uses fresh independent review and the same per-stage consent.
 async function nativeReady(role='reviewer') {const f=fixture();addNativeReview(f);f.tasks=[stoppedTask()];f.operators.set(20,operator(20,role));f.operators.get(20).recovery.actions[0].replacement.profiles.push('codex-review');await settle();await f.login();await open(f,role);await replaceWith(f,'codex-review');return f;}
 const native=await nativeReady();assert(!native.$('replacement-profile').children.find(o=>o.value==='codex-review').disabled);assert.match(native.$('replacement-session').textContent,/独立 checkout.*全新独立 Codex 会话.*不恢复旧会话/);assert(native.$('replacement-confirm-text').textContent.includes(nativeReviewWarning));assert.match(native.$('replacement-permission-note').textContent,/不等同于严格无执行审查/);assert(native.$('replacement-permission').children.find(o=>o.value==='codex_full_access').disabled);
 await confirm(native);assert.equal(native.posts.length,0);assert.match(native.$('retry-dialog-error').textContent,/精确权限范围/);await consent(native);await confirm(native);assert.deepEqual(native.posts[0].replacement,{profile:'codex-review',native_permission:nativeReviewMode,confirm_permission_expansion:true});assert(native.posts[0].revalidate_tests);assert.equal(native.challenges[0].action,'continue_review');assert.match(native.posts[0].permission_challenge,/replacement-token/);
 const nativeDeveloper=await nativeReady('developer');assert(nativeDeveloper.$('replacement-permission').children.find(o=>o.value===nativeReviewMode).disabled);await confirm(nativeDeveloper);assert.equal(nativeDeveloper.posts.length,0);assert.match(nativeDeveloper.$('retry-dialog-error').textContent,/原生模式/);
 for(const change of [f=>f.change('replacement-profile','claude'),f=>f.change('replacement-model','beta'),f=>f.change('replacement-effort','low'),f=>f.change('replacement-model-source','manual'),f=>f.change('replacement-permission','codex_full_access'),async f=>{f.timeOffset+=301000;await f.$('refresh').emit('click')}]) {const f=await nativeReady();await choose(f);await consent(f);await change(f);assert(!f.$('replacement-confirm').checked);assert(f.$('replacement-confirm').disabled);}
 for(const patch of [v=>v.scope.role='developer',v=>v.scope.reviewer.profile='codex',v=>v.scope.reviewer.native_permission='codex_full_access',v=>v.scope.reviewer.provider='codex_cli',v=>v.scope.reviewer.model='changed-default',v=>v.scope.reviewer.effort='high']) {const f=await nativeReady();f.challengePatch=patch;await f.$('replacement-challenge').emit('click');assert(f.$('replacement-confirm').disabled);await confirm(f);assert.equal(f.posts.length,0);}
 const nativeFrozen=await nativeReady();await consent(nativeFrozen);nativeFrozen.mode='abort';await confirm(nativeFrozen);const nativeBody=clone(nativeFrozen.posts[0]);nativeFrozen.timeOffset+=301000;await open(nativeFrozen,'reviewer');nativeFrozen.mode='422';await confirm(nativeFrozen);assert.deepEqual(nativeFrozen.posts.at(-1),nativeBody);assert.equal(nativeFrozen.challenges.length,1);
 // Resolving only the current local consent validation removes its stale error.
 const resolved=await ready();await open(resolved);const ordinaryDescription=resolved.$('retry-dialog-description').textContent;await expanded(resolved);assert.match(resolved.$('retry-dialog-description').textContent,/已停止的开发阶段继续.*保留原轮次和剩余修复预算/);assert(!resolved.$('retry-dialog-description').textContent.includes('重新执行开发'));
 await confirm(resolved);const consentMessage=resolved.$('retry-dialog-error').textContent;assert.match(consentMessage,/精确权限范围/);assert(!resolved.$('retry-dialog-error').hidden);assert.equal(resolved.posts.length,0);
 resolved.challengePatch=value=>value.expires_at_unix_ms=1;await resolved.$('replacement-challenge').emit('click');const scopeError=resolved.$('replacement-challenge-status').textContent;assert.match(scopeError,/读取失败/);resolved.$('replacement-confirm').checked=true;await resolved.$('replacement-confirm').emit('change');assert.equal(resolved.$('retry-dialog-error').textContent,consentMessage);assert.equal(resolved.$('replacement-challenge-status').textContent,scopeError);
 resolved.challengePatch=null;await resolved.$('replacement-challenge').emit('click');assert(!resolved.$('retry-dialog-error').hidden,'reading a scope alone is not consent');resolved.$('replacement-confirm').checked=true;await resolved.$('replacement-confirm').emit('change');assert(resolved.$('retry-dialog-error').hidden);assert.equal(resolved.$('retry-dialog-error').textContent,'');
 // Another local validation is not erased by a valid consent checkbox.
 resolved.$('retry-quota').value='1';await resolved.$('retry-quota').emit('input');await consent(resolved);await confirm(resolved);const quotaError=resolved.$('retry-dialog-error').textContent;assert.match(quotaError,/容量必须/);await resolved.$('replacement-confirm').emit('change');assert.equal(resolved.$('retry-dialog-error').textContent,quotaError);
 resolved.$('retry-quota').value='';await resolved.$('retry-quota').emit('input');await consent(resolved);resolved.$('retry-replace').checked=false;await resolved.$('retry-replace').emit('change');assert.equal(resolved.$('retry-dialog-description').textContent,ordinaryDescription);
 await expanded(resolved);await consent(resolved);resolved.mode='abort';await confirm(resolved);const frozenBody=clone(resolved.posts[0]),unknownNotice=resolved.$('detail-error').textContent;assert.match(unknownNotice,/续接未确认/);await open(resolved);const frozenNotice=resolved.$('replacement-challenge-status').textContent;assert.match(frozenNotice,/保留原 challenge/);await resolved.$('replacement-confirm').emit('change');await resolved.$('replacement-challenge').emit('click');assert.equal(resolved.$('detail-error').textContent,unknownNotice);assert.equal(resolved.$('replacement-challenge-status').textContent,frozenNotice);assert.equal(resolved.posts.length,1);resolved.mode='422';await confirm(resolved);const serverNotice=resolved.$('detail-error').textContent;assert.match(serverNotice,/expired challenge after restart/);await open(resolved);await resolved.$('replacement-confirm').emit('change');assert.equal(resolved.$('detail-error').textContent,serverNotice);assert.deepEqual(resolved.posts.at(-1),frozenBody);
 // Changed relevant values invalidate the one-stage challenge and human consent.
 for(const change of [f=>f.change('replacement-profile','claude'),f=>f.change('replacement-model','beta'),f=>f.change('replacement-effort','low'),f=>f.change('replacement-permission','codex_workspace_write'),async f=>{f.$('retry-quota').value='800';await f.$('retry-quota').emit('input')},async f=>{f.$('review-focus').value='new';await f.$('review-focus').emit('input')},async f=>{f.cache.profiles=f.cache.profiles.map(e=>({...e,generation:2}));await f.$('capability-read').emit('click')},async f=>{f.operators.get(20).recovery.actions[0].max_quota_bytes=900;await f.$('operator-read').emit('click')},async f=>{f.timeOffset+=301000;await f.$('refresh').emit('click') }]) {
   const f=await ready();await open(f);await expanded(f);await choose(f);await consent(f);assert.match(f.$('replacement-confirm-text').textContent,/filesystem AND network.*no native approval prompts/);assert.equal(f.$('replacement-confirm-text').querySelectorAll('img').length,0);await change(f);assert(!f.$('replacement-confirm').checked);assert(f.$('replacement-confirm').disabled);
 }
 // Expired/mismatched scope cannot unlock confirmation; stale selection/task/auth cannot accept a reply.
 for(const patch of [v=>v.scope.predecessor_task_id=999,v=>v.scope.action='continue_review',v=>v.scope.role='reviewer',v=>v.scope.developer.profile='claude',v=>v.expires_at_unix_ms=1]) {
   const f=await ready();await open(f);await expanded(f);f.challengePatch=patch;await f.$('replacement-challenge').emit('click');assert(f.$('replacement-confirm').disabled);await confirm(f);assert.equal(f.posts.length,0);
 }
 const stale=await ready();await open(stale);await expanded(stale);stale.delay=url=>url.endsWith('/replacement-challenge');stale.ignoreAbort=true;const read=stale.$('replacement-challenge').emit('click');await settle();await stale.$('replacement-challenge').emit('click');assert.equal(stale.challenges.length,1);await stale.change('replacement-model-source','manual');stale.resolve('/api/tasks/20/replacement-challenge');await read;assert(stale.$('replacement-confirm').disabled);assert.equal(stale.$('replacement-challenge-scope').textContent,'');
 // An ambiguous POST is immutable across expiry/restart rejection, auth, logout and history.
 for(const auth of ['bearer','session','hybrid']) {
   const f=await ready('developer',auth);await open(f);await expanded(f);await choose(f);await consent(f);f.mode='abort';await confirm(f);const frozen=clone(f.posts[0]);assert.match(frozen.permission_challenge,/replacement-token/);
   f.timeOffset+=301000;f.cache.profiles=[];await f.$('capability-read').emit('click');await open(f);assert(f.$('replacement-profile').disabled);assert(f.$('replacement-confirm').disabled);f.mode='422';await confirm(f);assert.deepEqual(f.posts.at(-1),frozen);
   await open(f);f.mode='401';await confirm(f);assert(!f.$('auth-panel').hidden);assert.deepEqual(f.posts.at(-1),frozen);await f.login();await open(f);f.mode='abort';await confirm(f);await f.$('logout').emit('click');await f.login();await open(f);await confirm(f);assert.deepEqual(f.posts.at(-1),frozen);
   f.events.get('pagehide')();f.events.get('pageshow')({persisted:true});await settle();await f.login();await open(f);f.mode='success';await confirm(f);assert.deepEqual(f.posts.at(-1),frozen);
 }
 // Interrupted in-flight rejection is ambiguous after page restoration, so it cannot unlock a new intent.
 const interrupted=await ready();await open(interrupted);await replaceWith(interrupted,'claude');interrupted.mode='422';interrupted.delay=url=>url.endsWith('/retry');interrupted.ignoreAbort=true;const interruptedPost=confirm(interrupted);await settle();const interruptedBody=clone(interrupted.posts[0]);interrupted.events.get('pagehide')();interrupted.events.get('pageshow')({persisted:true});interrupted.resolve('/api/tasks/20/retry');await interruptedPost;interrupted.delay=()=>false;await interrupted.login();await open(interrupted);await confirm(interrupted);assert.deepEqual(interrupted.posts.at(-1),interruptedBody);await open(interrupted);assert(interrupted.$('replacement-profile').disabled);await interrupted.$('retry-dismiss').emit('click');
 // A logout or task switch while challenge resolution is pending never restores consent.
 for(const leave of [async f=>{await f.$('logout').emit('click')},async f=>{f.tasks.push(stoppedTask(21));f.operators.set(21,operator(21));await f.$('refresh').emit('click');await select(f,21)}]) {
   const f=await ready();await open(f);await expanded(f);f.delay=url=>url.endsWith('/replacement-challenge');f.ignoreAbort=true;const pending=f.$('replacement-challenge').emit('click');await settle();await leave(f);f.resolve('/api/tasks/20/replacement-challenge');await pending;assert(!f.$('retry-dialog').open);assert(f.$('replacement-confirm').disabled);assert.equal(f.$('replacement-challenge-scope').textContent,'');
 }
 // The authoritative reservation wins, but a matching reservation never drops our original token.
 const same=await ready();await open(same);await expanded(same);await consent(same);same.mode='abort';await confirm(same);const body=clone(same.posts[0]);same.operators.get(20).recovery.reserved_request=saved(body);await same.$('operator-read').emit('click');await open(same);await confirm(same);assert.deepEqual(same.posts.at(-1),body);
 const winner=await ready();await open(winner);await expanded(winner);await consent(winner);winner.mode='abort';await confirm(winner);await open(winner);const accepted={key:'other-tab',replacement:{profile:'claude',model:{value:'winner-model',source:'manual'}}};winner.operators.get(20).recovery.reserved_request=saved(accepted);await winner.$('operator-read').emit('click');assert.match(winner.$('replacement-defaults').textContent,/winner-model/);assert(winner.$('replacement-profile').disabled);winner.mode='success';await confirm(winner);assert.equal(winner.posts.at(-1).key,'other-tab');assert(!winner.posts.at(-1).permission_challenge);assert.match(winner.$('continuation-choice').textContent,/winner-model/);
 // A stale tab POST may lose to a different replacement or ordinary mode; show actual returned winner.
 const race=await ready();await open(race);await replaceWith(race,'claude');race.winner={key:'other-tab',path:'/api/tasks/20/retry'};await confirm(race);assert.match(race.$('continuation-choice').textContent,/无替换.*其他页面/);
 // Duplicate submission and newer navigation do not mutate the frozen request or steal selection.
 const navigation=await ready();navigation.tasks.push(stoppedTask(21));navigation.operators.set(21,operator(21));await navigation.$('refresh').emit('click');await select(navigation,20);await open(navigation);await replaceWith(navigation);navigation.delay=url=>url.endsWith('/retry');const post=confirm(navigation);await settle();await confirm(navigation);assert.equal(navigation.posts.length,1);await select(navigation,21);navigation.resolve('/api/tasks/20/retry');await post;assert.equal(navigation.$('detail-title').textContent,'任务 #21');
 const identity=await ready();await open(identity);await replaceWith(identity,'claude');identity.mode='abort';await confirm(identity);await identity.$('logout').emit('click');identity.tasks[0].key='different-database-task';await identity.login();assert(identity.$('retry-task').disabled);assert.match(identity.$('operator-recovery').textContent,/任务身份.*不同/);await identity.$('retry-task').emit('click');assert(!identity.$('retry-dialog').open);assert.equal(identity.posts.length,1);
 const dismiss=await ready();await open(dismiss);await expanded(dismiss);await consent(dismiss);await dismiss.$('retry-dismiss').emit('click');await open(dismiss);assert(!dismiss.$('retry-replace').checked);assert(!dismiss.$('replacement-confirm').checked);assert.equal(dismiss.posts.length,0);
 assert(!ordinary.requests.some(r=>r.url.endsWith('/refresh')||r.url.startsWith('/api/resources')));

 // Kiro replacement uses a fresh invocation, explicit host opt-in and its own permission scope.
 async function kiroReady(role='developer',allowed=true) {const f=fixture();addKiro(f,allowed);f.tasks=[stoppedTask()];f.operators.set(20,operator(20,role));f.operators.get(20).recovery.actions[0].replacement.profiles.push('kiro');await settle();await f.login();await open(f,role);await replaceWith(f,'kiro');return f;}
 const kiro=await kiroReady();assert.match(kiro.$('replacement-session').textContent,/Kiro.*不恢复原生会话/);assert.match(kiro.$('replacement-defaults').textContent,/暂不支持/);await choose(kiro,'alpha','');assert(kiro.$('replacement-effort').disabled);assert.equal(kiro.$('replacement-effort').children.length,1);assert.match(kiro.$('replacement-effort').textContent,/暂不支持 effort/);
 await kiro.change('replacement-permission','kiro_workspace_write');for(const text of ['edit-workspace','不是 OS 沙箱','hooks','MCP','settings','拒绝交互权限请求','不启用 trust-all 或 dev-shell'])assert(kiro.$('replacement-confirm-text').textContent.includes(text),text);
 await confirm(kiro);assert.equal(kiro.posts.length,0);await consent(kiro);await kiro.change('replacement-model','beta');assert(!kiro.$('replacement-confirm').checked);await confirm(kiro);assert.equal(kiro.posts.length,0);await consent(kiro);await confirm(kiro);assert.equal(kiro.posts.length,1);assert.equal(kiro.posts[0].replacement.native_permission,'kiro_workspace_write');assert(kiro.posts[0].replacement.confirm_permission_expansion);assert(!Object.hasOwn(kiro.posts[0].replacement,'effort'));assert.equal(kiro.challenges.at(-1).replacement.native_permission,'kiro_workspace_write');
 const kiroBlocked=await kiroReady('developer',false);assert(kiroBlocked.$('replacement-permission').children.find(o=>o.value==='kiro_workspace_write').disabled);await kiroBlocked.change('replacement-permission','kiro_workspace_write');await confirm(kiroBlocked);assert.equal(kiroBlocked.posts.length,0);
 const kiroReviewer=await kiroReady('reviewer');assert(kiroReviewer.$('replacement-profile').children.find(o=>o.value==='kiro').disabled);await confirm(kiroReviewer);assert.equal(kiroReviewer.posts.length,0);assert.match(kiroReviewer.$('retry-dialog-error').textContent,/Kiro.*不支持审查/);
 console.log('PASS: Kiro developer-only fresh invocation, opt-in workspace editing consent, no effort or reviewer; opt-in Codex native review replacement consent, independent-session warning, role/default separation, scope/expiry/change gates and frozen replay; stage replacement UI server-owned eligibility/legacy unavailability; optional ordinary path; native catalog/manual/mode safety; exact reviewer candidate/test-once disclosure; original repair budget; scoped expiring consent; stale challenge and navigation fences; immutable auth/logout/history replay; matching reservation token retention and different first-winner display; duplicate guard; no automatic probes');
})().catch(error=>{console.error(error);process.exitCode=1});
