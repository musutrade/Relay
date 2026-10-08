// Run with: node app/tests/ui_publication_test.cjs
// Actual inline UI code, deterministic clock/network, no browser or external CLI required.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const {webcrypto} = require('node:crypto');
const html = fs.readFileSync(require('node:path').resolve(__dirname, '../static/index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const clone = value => JSON.parse(JSON.stringify(value));
const settle = () => new Promise(resolve => setImmediate(resolve));
const task = (id, outcome = 'success') => ({id, key: 'task-' + id, payload: JSON.stringify({repository:'repo',requirements:'fixture',agent:'agent',test:null,publish:false,workspace_quota_bytes:100}), state:outcome ? 'finished':'queued', generation:outcome ? 1:0, owner:outcome ? 'host':null, result:outcome ? JSON.stringify({outcome,workspace:'/fixture',draft_pr:null}) : null, continuation_status:null});
const action = () => ({id:'publish_approved',allowed:true,ordinary_allowed:true,candidate_sha:'a'.repeat(40),base_sha:'b'.repeat(40),github_repository:'owner/project',base_branch:'main',draft_pr_adapter:'publish',publisher_binding:'c'.repeat(64),draft:true,dry_run:false,requires_prior_test_acceptance:true,authorization_ttl_seconds:86400,expires_at_unix_seconds:null});
const operator = t => ({task_id:t.id,generation:t.generation,failure:null,resources:{usage:{logical_bytes:150,complete:true,measured_at:1700000000,reason:null},quota_bytes:1000,host_policy_cap_bytes:1000,snapshot_cap_bytes:800,enforcement:'logical_bytes_best_effort',os_hard_quota:false,disk_reserved:false},retained_result:{available:Boolean(t.result),immutable:true},workspace_retained:true,recovery:{inherited_quota_bytes:1000,actions:t.result?[action()]:[],blocked_reason:null,successor_id:null,reserved_request:null}});
const estimate = () => ({host_policy_cap_bytes:1000,default_quota_bytes:1000,snapshot_cap_bytes:800,initial_estimate:{source:'host_inventory',snapshot_bytes:100,git_metadata_reference_bytes:20,reviewer_copy_bytes:100,estimated_initial_bytes:220,complete:true,notes:['initial inventory only']},build_growth:'unknown',enforcement:'logical_bytes_best_effort',os_hard_quota:false,disk_reserved:false});
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
  const f = {nodes, events, timers, requests:[], pending:[], posts:[], tasks:[task(2,null),task(1)], operators:new Map(), estimate:estimate(), status:{active:null,recovery_required:false,diagnostic:null}, postMode:'success', operatorError:0, estimateError:0, delay:()=>false, ignoreAbort:false, responseQuota:null,logoutError:false,responseMismatch:false,failureCode:null};
  for (const t of f.tasks) f.operators.set(t.id,operator(t));
  const $ = id => nodes.get(id); f.$ = $;
  f.resourceReads = () => f.requests.filter(r=>r.url.startsWith('/api/resources')||r.url.endsWith('/operator'));
  f.login = async () => { $('token').value='test-token'; $('username').value='operator'; $('password').value='password'; await $('auth-form').emit('submit'); await settle(); };
  f.select = async id => { await $('task-list').querySelectorAll('button').find(b=>b.dataset.taskId===String(id)).emit('click'); await settle(); };
  f.resolve = url => { const i=f.pending.findIndex(p=>p.url===url); assert(i>=0,'missing '+url); f.pending.splice(i,1)[0].resolve(); };
  f.flush = async () => { for (const [id,timer] of [...timers]) if(timer.milliseconds===2000){timers.delete(id);timer.callback();} await settle(); };
  async function fetch(url, opts) {
    f.requests.push({url,opts}); let data,status=200;
    assert.equal(opts.headers.Authorization,authMode==='bearer'&&url.startsWith('/api/')?'Bearer test-token':undefined);
    if(url==='/auth/status')data={mode:authMode,authenticated:false};
    else if(url==='/auth/login')data={authenticated:true};
    else if(url==='/auth/logout'){if(f.logoutError)throw new TypeError('logout failed');data={authenticated:false};}
    else if(url.endsWith('/merge-preview'))data={eligible:false,reason:'Merge disabled in fixture',policies:[],authorizations:[],risk_disclosure:null,lane_diagnostic:null};
    else if(url.endsWith('/merge-authorizations'))data=[];
    else if (url==='/api/config')data={repositories:['repo','other'],agents:['agent'],tests:['test'],workflows:[{name:'review / flow',repository:'repo',developer:'agent',reviewer:'agent',test:'test',max_repairs:0}]};
    else if(url==='/api/status')data=f.status;
    else if(url==='/api/tasks'&&opts.method==='GET')data=f.tasks;
    else if(url.startsWith('/api/resources?')){data=f.estimate;if(f.estimateError){status=f.estimateError;data={error:'estimate unavailable'};}}
    else if(url.endsWith('/operator')){data=f.operators.get(Number(url.split('/').at(-2)));if(f.operatorError){status=f.operatorError;data={error:'operator unavailable'};}}
    else if(opts.method==='POST'){
      const body=JSON.parse(opts.body); f.posts.push({url,body});
      if(f.postMode==='abort')throw new TypeError('lost response');
      if(['401','409','422'].includes(f.postMode)){status=Number(f.postMode);data={error:'host policy changed',failure:f.failureCode?{code:f.failureCode}:null};}
      else {
        assert(url.endsWith('/publish-approved'));
        const parent=Number(url.split('/').at(-2)),old=f.tasks.find(t=>t.id===parent);
        data=f.tasks.find(t=>JSON.parse(t.payload).continuation?.predecessor_task_id===parent);
        if(!data){data={...task(Math.max(...f.tasks.map(t=>t.id))+1,null),key:body.key,payload:JSON.stringify({...JSON.parse(old.payload),continuation:{predecessor_task_id:parent,publish_approved:{request:body}}})};f.tasks.unshift(data);f.operators.set(data.id,operator(data));}
        if(f.postMode!=='unverifiable'){
          old.continuation_status={successor_id:data.id};f.operators.get(parent).recovery.successor_id=data.id;f.operators.get(parent).recovery.actions=[];
        }
        if(f.postMode==='lost_after_commit')throw new TypeError('committed, response lost');
        if(f.responseMismatch)data={...data,key:'wrong-response-key'};

      }
    } else data=f.tasks.find(t=>t.id===Number(url.split('/').at(-1)));
    const snapshot=clone(data),response={ok:status===200,status,json:async()=>clone(snapshot)};
    if(f.delay(url,opts))return new Promise((resolve,reject)=>{f.pending.push({url,resolve:()=>resolve(response)});if(!f.ignoreAbort)opts.signal.addEventListener('abort',()=>{const e=new Error('abort');e.name='AbortError';reject(e);});});
    return response;
  }
  const context = {console,document,window:{matchMedia:()=>({matches:false,addEventListener(){}}),addEventListener:(name,fn)=>events.set(name,fn)},navigator:{},fetch,crypto:webcrypto,AbortController,TextEncoder,Uint8Array,Date,Error,JSON,Array,String,Number,Boolean,Set,Map,encodeURIComponent,setTimeout:(callback,milliseconds)=>{const id=++timerId;timers.set(id,{callback,milliseconds});return id;},clearTimeout:id=>timers.delete(id)};
  Object.defineProperties(context,{localStorage:{get(){throw Error('must not use storage')}},sessionStorage:{get(){throw Error('must not use storage')}}});
  vm.runInNewContext(script,context);return f;
}
async function ready(mode='bearer') { const f=fixture(mode); await settle(); await f.login(); await f.select(1); return f; }
async function approve(f) { await f.$('publish-task').emit('click'); f.$('publish-accept-tests').checked=true; await f.$('publish-accept-tests').emit('change'); return f.$('publish-confirm').emit('click'); }
(async()=>{
  const f=await ready(),$=f.$;
  assert(!$('publish-task').hidden);assert(!$('publish-task').disabled);assert($('retry-task').hidden);assert($('review-task').hidden);assert.equal(f.posts.length,0);
  await $('publish-task').emit('click');assert($('publish-dialog').open);assert($('publish-confirm').disabled);assert.equal($('publish-accept-tests').checked,false);
  for(const text of ['a'.repeat(40),'b'.repeat(40),'owner/project','main','publish','c'.repeat(64),'Draft PR','不会合并'])assert($('publish-scope').textContent.includes(text),text);
  assert.match($('publish-expiry').textContent,/24 小时/);
  assert.match(html,/外部输入未冻结，也不会重新验证/);
  await $('publish-confirm').emit('click');assert.equal(f.posts.length,0);assert.match($('publish-dialog-error').textContent,/明确接受/);
  $('publish-accept-tests').checked=true;await $('publish-accept-tests').emit('change');assert(!$('publish-confirm').disabled);
  await $('publish-dismiss').emit('click');assert(!$('publish-dialog').open);assert.equal($('publish-scope').textContent,'');assert.equal($('publish-accept-tests').checked,false);assert.equal(f.posts.length,0);
  await $('publish-task').emit('click');assert($('publish-confirm').disabled);await $('publish-dialog').emit('cancel');assert(!$('publish-dialog').open);assert.equal(f.posts.length,0);
  // A dialog never survives navigation, changed host policy, logout, or history traversal.
  await $('publish-task').emit('click');$('publish-accept-tests').checked=true;await f.select(2);await $('publish-confirm').emit('click');assert.equal(f.posts.length,0);assert(!$('publish-dialog').open);
  await f.select(1);await $('publish-task').emit('click');f.operators.get(1).recovery.actions[0].base_branch='release';await $('operator-read').emit('click');assert(!$('publish-dialog').open);await $('publish-confirm').emit('click');assert.equal(f.posts.length,0);
  await $('publish-task').emit('click');assert.match($('publish-scope').textContent,/release/);f.events.get('popstate')();assert(!$('publish-dialog').open);
  await $('publish-task').emit('click');await $('logout').emit('click');assert(!$('publish-dialog').open);assert.equal($('publish-scope').textContent,'');await $('publish-confirm').emit('click');assert.equal(f.posts.length,0);
  await f.login();await f.select(1);await $('publish-task').emit('click');f.events.get('pagehide')();assert(!$('publish-dialog').open);f.events.get('pageshow')({persisted:true});await f.login();await f.select(1);
  // One frozen wire request survives unknown responses, repeat clicks, and authentication loss.
  f.postMode='abort';await approve(f);assert.equal(f.posts.length,1);const first=clone(f.posts[0]);assert.equal(first.url,'/api/tasks/1/publish-approved');assert.equal(first.body.confirm_publish,true);assert.equal(first.body.accept_prior_test_evidence,true);assert.equal(first.body.base_branch,'release');assert.match($('detail-error').textContent,/尚未确认/);assert.equal(f.posts.length,1,'no automatic retry');
  await $('refresh').emit('click');await f.flush();assert.equal(f.posts.length,1,'polling cannot resubmit');await approve(f);assert.deepEqual(f.posts[1],first);
  f.postMode='401';await approve(f);assert.deepEqual(f.posts[2],first);assert(!$('auth-panel').hidden);assert(!$('publish-dialog').open);await f.login();await f.select(1);
  f.postMode='422';await approve(f);assert.deepEqual(f.posts[3],first);assert.match($('detail-error').textContent,/尚未确认/,'later rejection cannot erase ambiguity');
  f.postMode='success';f.delay=(url,opts)=>url.endsWith('/publish-approved')&&opts.method==='POST';const pending=approve(f);await settle();const count=f.posts.length;await $('publish-task').emit('click');await $('publish-confirm').emit('click');assert.equal(f.posts.length,count);assert.deepEqual(f.posts.at(-1),first);f.resolve(first.url);await pending;f.delay=()=>false;
  assert.match($('continuation-choice').textContent,/不重跑开发、审查或测试/);assert.match($('detail-meta').textContent,/仅发布已批准候选/);assert.match($('continuation-choice').textContent,/不代表已合并/);await f.select(1);assert($('publish-task').hidden);assert(!$('continuation-next').hidden);assert.equal(f.tasks.length,3);
  // Provenance is derived from durable payload, so refresh/auth restoration cannot make copied token/model evidence look new.
  await $('logout').emit('click');await f.login();await f.select(3);assert.match($('continuation-choice').textContent,/继承自任务 #1/);assert.match($('continuation-choice').textContent,/不表示本次产生了新的开发、审查或测试调用/);
  // Lost response after commit is reconciled by read-only successor discovery, not a second POST.
  const lost=await ready();lost.postMode='lost_after_commit';await approve(lost);assert.equal(lost.posts.length,1);await lost.$('operator-read').emit('click');assert(lost.$('publish-task').hidden);assert(!lost.$('continuation-next').hidden);await lost.$('publish-task').emit('click');assert.equal(lost.posts.length,1);
  // An HTTP success carrying the wrong task identity is still an unknown response, never a completion claim.
  const mismatch=await ready();mismatch.postMode='unverifiable';mismatch.responseMismatch=true;await approve(mismatch);assert.match(mismatch.$('detail-error').textContent,/尚未确认/);assert.equal(mismatch.$('detail-title').textContent,'任务 #1');const unverified=clone(mismatch.posts[0]);mismatch.responseMismatch=false;await approve(mismatch);assert.deepEqual(mismatch.posts[1],unverified);assert.equal(mismatch.$('detail-title').textContent,'任务 #3');
  // First definitive rejection can be corrected; unknown request identity or changed scope cannot.
  const rejected=await ready();rejected.postMode='422';await approve(rejected);const rejectedKey=rejected.posts[0].body.key;rejected.postMode='success';await rejected.$('operator-read').emit('click');await approve(rejected);assert.notEqual(rejected.posts[1].body.key,rejectedKey);
  const changed=await ready();changed.postMode='abort';await approve(changed);changed.operators.get(1).recovery.actions[0].github_repository='other/repository';await changed.$('operator-read').emit('click');assert(changed.$('publish-task').disabled);await approve(changed);assert.equal(changed.posts.length,1);assert.match(changed.$('operator-recovery').textContent,/不可改目标/);
  // Only a structured, fresh key conflict releases an unusable key; uncertain retries retain it.
  const conflict=await ready();conflict.postMode='409';conflict.failureCode='idempotency_conflict';await approve(conflict);const conflicted=clone(conflict.posts[0]);assert.match(conflict.$('detail-error').textContent,/发布请求被拒绝/);conflict.postMode='success';await conflict.$('operator-read').emit('click');await approve(conflict);assert.notEqual(conflict.posts[1].body.key,conflicted.body.key);
  for(const failureCode of [null,'publication_reservation_conflict']){const generic=await ready();generic.postMode='409';generic.failureCode=failureCode;await approve(generic);const frozen=clone(generic.posts[0]);await approve(generic);assert.deepEqual(generic.posts[1],frozen);assert.match(generic.$('detail-error').textContent,/尚未确认/);}
  const ambiguousConflict=await ready();ambiguousConflict.postMode='abort';await approve(ambiguousConflict);const ambiguousBody=clone(ambiguousConflict.posts[0]);ambiguousConflict.postMode='409';ambiguousConflict.failureCode='idempotency_conflict';await approve(ambiguousConflict);await approve(ambiguousConflict);assert.deepEqual(ambiguousConflict.posts[2],ambiguousBody);assert.match(ambiguousConflict.$('detail-error').textContent,/尚未确认/);
  // Server-owned eligibility is authoritative: no inferred action from a successful result.
  for(const reason of ['发布已尝试，外部结果未知，禁止重放','授权已过期','工作区已清理']){const g=await ready();g.operators.get(1).recovery.actions=[];g.operators.get(1).recovery.blocked_reason=reason;await g.$('operator-read').emit('click');assert(g.$('publish-task').hidden);assert(g.$('operator-recovery').textContent.includes(reason));await approve(g);assert.equal(g.posts.length,0);}
  for(const patch of [{draft:false},{allowed:false},{requires_prior_test_acceptance:false},{candidate_sha:'bad'},{publisher_binding:''},{expires_at_unix_seconds:Math.floor(Date.now()/1000)-1}]){const g=await ready();Object.assign(g.operators.get(1).recovery.actions[0],patch);await g.$('operator-read').emit('click');assert(g.$('publish-task').hidden);await approve(g);assert.equal(g.posts.length,0);}
  const unknown=await ready();unknown.status.recovery_required=true;await unknown.$('refresh').emit('click');assert(unknown.$('publish-task').disabled||unknown.$('publish-task').hidden);await approve(unknown);assert.equal(unknown.posts.length,0);
  // A saved server reservation can be recovered only after inspecting the complete frozen scope.
  const reserved=await ready(),a=reserved.operators.get(1).recovery.actions[0];a.expires_at_unix_seconds=Math.floor(Date.now()/1000)+3600;
  const body={key:'server-reservation',confirm_publish:true,accept_prior_test_evidence:true,...Object.fromEntries(['candidate_sha','github_repository','base_branch','draft_pr_adapter','publisher_binding'].map(k=>[k,a[k]]))};reserved.operators.get(1).recovery.reserved_request={action_id:'publish_approved',...body};await reserved.$('operator-read').emit('click');await reserved.$('publish-task').emit('click');assert.match(reserved.$('publish-expiry').textContent,/重试不会延长授权/);assert(reserved.$('publish-confirm').disabled);reserved.$('publish-accept-tests').checked=true;await reserved.$('publish-accept-tests').emit('change');await reserved.$('publish-confirm').emit('click');assert.deepEqual(reserved.posts[0].body,body);
  // Untrusted text remains text, and dry-run is unmistakable at confirmation time.
  const safe=await ready(),attack='<img src=x onerror=alert(1)>';safe.operators.get(1).recovery.actions[0].base_branch=attack;safe.operators.get(1).recovery.actions[0].dry_run=true;await safe.$('operator-read').emit('click');await safe.$('publish-task').emit('click');assert(safe.$('publish-scope').textContent.includes(attack));assert.equal(safe.$('publish-scope').querySelectorAll('img').length,0);assert.match(safe.$('publish-scope').textContent,/Dry run（不会推送或创建 PR）/);
  // Late publication responses may update the task list, but must never hijack current navigation.
  const late=await ready();late.delay=(url,opts)=>opts.method==='POST'&&url.endsWith('/publish-approved');late.ignoreAbort=true;const response=approve(late);await settle();await late.select(2);late.resolve('/api/tasks/1/publish-approved');await response;assert.equal(late.$('detail-title').textContent,'任务 #2');
  for(const mode of ['session','hybrid']){const g=await ready(mode);await g.$('publish-task').emit('click');g.$('publish-accept-tests').checked=true;g.logoutError=true;await g.$('logout').emit('click');assert(!g.$('publish-dialog').open,'even an uncertain logout discards unsubmitted consent');assert.equal(g.posts.length,0);g.logoutError=false;await g.$('logout').emit('click');await g.login();await g.select(1);g.postMode='abort';await approve(g);const frozen=clone(g.posts[0]);await g.$('logout').emit('click');await g.login();await g.select(1);await approve(g);assert.deepEqual(g.posts[1],frozen);}
  // A stale response arriving after logout cannot repopulate private UI or trigger publication.
  const auth=await ready();auth.delay=(url,opts)=>opts.method==='POST'&&url.endsWith('/publish-approved');auth.ignoreAbort=true;const responseAfterLogout=approve(auth);await settle();await auth.$('logout').emit('click');auth.resolve('/api/tasks/1/publish-approved');await responseAfterLogout;assert(auth.$('task-detail').hidden);assert.equal(auth.$('publish-scope').textContent,'');assert.equal(auth.posts.length,1);
  console.log('PASS: publication UI exact scope, explicit test acceptance, idempotent retries, no automatic replay, cancel/history/navigation/auth fencing, reserved requests, draft/dry-run wording and safe text');
})().catch(error=>{console.error(error);process.exitCode=1});
