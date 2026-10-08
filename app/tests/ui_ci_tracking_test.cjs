// Run with: node app/tests/ui_ci_tracking_test.cjs
// Actual inline UI code, deterministic clock/network, no browser or external CLI required.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const {webcrypto} = require('node:crypto');
const Date = class extends globalThis.Date { constructor(...args) { super(...(args.length ? args : [1800000000000])); } static now() { return 1800000000000; } };
const html = fs.readFileSync(require('node:path').resolve(__dirname, '../static/index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const clone = value => JSON.parse(JSON.stringify(value));
const settle = () => new Promise(resolve => setImmediate(resolve));
const task = (id, outcome = 'success') => ({id, key: 'task-' + id, payload: JSON.stringify({repository:'repo',requirements:'fixture',agent:'agent',test:null,publish:false,workspace_quota_bytes:100}), state:outcome ? 'finished':'queued', generation:outcome ? 1:0, owner:outcome ? 'host':null, result:outcome ? JSON.stringify({outcome,workspace:'/fixture',draft_pr:null}) : null, continuation_status:null});
const publication = () => ({repository:'owner/project',base_branch:'main',head_sha:'a'.repeat(40),head_branch:'relay/candidate',pr_number:19,pr_url:'https://github.com/owner/project/pull/19'});
const policy = () => ({name:'required-ci',policy_digest:'d'.repeat(64),workflow_id:41,app_id:15368,event:'pull_request',required_jobs:['check','browser'],poll_interval_seconds:60,observation_window_seconds:3600});
const preview = () => ({eligible:true,reason:null,publication:publication(),policies:[policy()],tracks:[],remote_merge_eligibility:'not_established'});
const track = (status='watching') => ({...publication(),...policy(),id:10,publication_task_id:1,policy:'required-ci',status,attempt:0,revision:1,stop_requested:false,last_observed_at:null,window_generation:1,window_started_at:Math.floor(Date.now()/1000),created_at:Math.floor(Date.now()/1000),deadline:Math.floor(Date.now()/1000)+3600,next_poll_at:Math.floor(Date.now()/1000)+60,latest_evidence:null,diagnostic:null,observed_repository_id:null,observed_pr_id:null,observed_base_sha:null,remote_merge_eligibility:'not_established'});
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
  const f = {nodes, events, timers, requests:[], pending:[], posts:[], tasks:[task(2,null),task(1)], operators:new Map(), estimate:estimate(), status:{active:null,recovery_required:false,diagnostic:null}, postMode:'success', operatorError:0, estimateError:0, delay:()=>false, ignoreAbort:false, responseQuota:null,logoutError:false,responseMismatch:false,failureCode:null,ci:preview(),ciError:0};
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
    else if(url==='/api/config')data={repositories:['repo','other'],agents:['agent'],tests:['test'],workflows:[{name:'review / flow',repository:'repo',developer:'agent',reviewer:'agent',test:'test',max_repairs:0}]};
    else if(url==='/api/status')data=f.status;
    else if(url==='/api/tasks'&&opts.method==='GET')data=f.tasks;
    else if(url.startsWith('/api/resources?')){data=f.estimate;if(f.estimateError){status=f.estimateError;data={error:'estimate unavailable'};}}
    else if(url.endsWith('/ci-preview')){data=Number(url.split('/').at(-2))===1?f.ci:{eligible:false,reason:'No real publication',publication:null,policies:[],tracks:[],remote_merge_eligibility:'not_established'};if(f.ciError){status=f.ciError;data={error:'CI unavailable'};}}
    else if(url.endsWith('/operator')){data=f.operators.get(Number(url.split('/').at(-2)));if(f.operatorError){status=f.operatorError;data={error:'operator unavailable'};}}
    else if(opts.method==='POST'){
      const body=JSON.parse(opts.body); f.posts.push({url,body});
      if(f.postMode==='abort')throw new TypeError('lost response');
      if(['401','409','422'].includes(f.postMode)){status=Number(f.postMode);data={error:'host policy changed',failure:f.failureCode?{code:f.failureCode}:null};}
      else {
        if(url.endsWith('/track-ci')){
          data=f.ci.tracks[0]||track();
          if(f.postMode!=='unverifiable')f.ci.tracks=[data];
        }else{
          const previous=f.ci.tracks[0];assert(previous);assert.equal(body.expected_revision,previous.revision);
          data={...previous,revision:previous.revision+1,status:url.endsWith('/stop')?'stopped':'watching',stop_requested:url.endsWith('/stop'),window_generation:previous.window_generation+(url.endsWith('/resume')?1:0),window_started_at:url.endsWith('/resume')?Math.floor(Date.now()/1000):previous.window_started_at,deadline:url.endsWith('/resume')?Math.floor(Date.now()/1000)+previous.observation_window_seconds:previous.deadline};
          if(f.postMode!=='unverifiable')f.ci.tracks=[data];
        }
        if(f.postMode==='lost_after_commit')throw new TypeError('committed, response lost');
        if(f.responseMismatch)data={...data,head_sha:'b'.repeat(40)};

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
const buttons = f => f.$('ci-tracks').querySelectorAll('button');
const control = (f, action) => buttons(f).find(button => button.dataset.ciAction === action);
(async()=>{
  const f=await ready(),$=f.$;
  assert(!$('detail-ci').hidden);assert(!$('ci-start').disabled);assert.equal($('ci-policy').value,'required-ci');assert.equal(f.posts.length,0);
  for(const text of ['a'.repeat(40),'owner/project','main','#19'])assert($('ci-scope').textContent.includes(text),text);
  for(const text of ['Workflow ID：41','App ID：15368','pull_request','check','browser'])assert($('ci-policy-scope').textContent.includes(text),text);
  await $('refresh').emit('click');await f.flush();assert.equal(f.posts.length,0,'local reads never start an observer');
  await $('ci-start').emit('click');assert.equal(f.posts.length,1);assert.deepEqual(Object.keys(f.posts[0].body).sort(),['key','policy','policy_digest']);assert($('ci-start').hidden);assert.match($('ci-tracks').textContent,/正在跟踪/);assert(control(f,'stop'));assert(!control(f,'resume'));
  await control(f,'stop').emit('click');assert.equal(f.posts[1].url,'/api/ci-tracks/10/stop');assert.deepEqual(f.posts[1].body,{expected_revision:1});assert.match($('ci-tracks').textContent,/已停止/);assert(control(f,'resume'));
  await control(f,'resume').emit('click');assert.deepEqual(f.posts[2].body,{expected_revision:2});assert.match($('ci-tracks').textContent,/正在跟踪/);assert(!control(f,'resume'));
  for(const [status,label,resumable] of [['configured_checks_passed','configured checks passed; remote merge eligibility not established',false],['checks_failed','配置检查失败',true],['blocked','跟踪受阻',true],['process_unknown','本机核对',false],['expired','观察期限已到',true],['stopped','已停止',true],['pr_closed','PR 已关闭',false],['pr_merged','非 Relay 合并',false]]){
    f.ci.tracks[0]={...f.ci.tracks[0],status,revision:f.ci.tracks[0].revision+1,diagnostic:{code:'fixture',message:'Actionable fixture diagnostic'}};
    await $('ci-read').emit('click');assert($('ci-tracks').textContent.includes(label),status);assert($('ci-tracks').textContent.includes('Actionable fixture diagnostic'));assert.equal(Boolean(control(f,'resume')),resumable,status);assert(!control(f,'stop'));
  }
  f.ci.tracks[0]={...f.ci.tracks[0],status:'watching',latest_evidence:{version:1,observation:'ok',complete:true,remote_merge_eligibility:'not_established',observed_at:Math.floor(Date.now()/1000),missing_jobs:['browser'],failed_jobs:[],run:{id:500,run_attempt:2,jobs:[{name:'check',status:'in_progress',conclusion:null}]}}};
  await $('ci-read').emit('click');assert.match($('ci-tracks').textContent,/缺失检查：browser/);assert.match($('ci-tracks').textContent,/等待中检查：check/);assert.match($('ci-tracks').textContent,/Actions run #500/);assert.match($('ci-tracks').textContent,/最新观察/);
  f.ci.tracks[0].status='checks_failed';f.ci.tracks[0].latest_evidence.failed_jobs=['check'];f.ci.tracks[0].latest_evidence.run.jobs[0]={name:'check',status:'completed',conclusion:'failure'};await $('ci-read').emit('click');assert.match($('ci-tracks').textContent,/失败检查：check/);
  assert.match(html,/Draft PR 保持 draft/);assert.match(html,/未证明全部分支规则通过/);assert(!/id="ci-[^"]+"[^>]*type="(?:text|checkbox)"/.test(html),'no SHA input or added confirmation step');
  // Exact replay is explicit and remains frozen across read, timeout, rejection and authentication loss.
  const lost=await ready();lost.postMode='abort';await lost.$('ci-start').emit('click');const first=clone(lost.posts[0]);assert.match(lost.$('ci-error').textContent,/尚未确认/);assert(lost.$('ci-policy').disabled);
  await lost.$('refresh').emit('click');await lost.flush();assert.equal(lost.posts.length,1);await lost.$('ci-start').emit('click');assert.deepEqual(lost.posts[1],first);
  lost.postMode='401';await lost.$('ci-start').emit('click');assert(!lost.$('auth-panel').hidden);assert(lost.$('detail-ci').hidden);await lost.login();await lost.select(1);
  lost.postMode='422';await lost.$('ci-start').emit('click');assert.deepEqual(lost.posts.at(-1),first);lost.postMode='success';await lost.$('ci-start').emit('click');assert.deepEqual(lost.posts.at(-1),first);assert(lost.$('ci-start').hidden);
  const committed=await ready();committed.postMode='lost_after_commit';await committed.$('ci-start').emit('click');await committed.$('ci-read').emit('click');assert(committed.$('ci-start').hidden);assert.equal(committed.posts.length,1);
  const mismatch=await ready();mismatch.postMode='unverifiable';mismatch.responseMismatch=true;await mismatch.$('ci-start').emit('click');assert.match(mismatch.$('ci-error').textContent,/尚未确认/);assert.equal(mismatch.$('ci-tracks').children.length,0);const fixed=clone(mismatch.posts[0]);mismatch.responseMismatch=false;await mismatch.$('ci-start').emit('click');assert.deepEqual(mismatch.posts[1],fixed);
  const duplicate=await ready();duplicate.delay=(_url,opts)=>opts.method==='POST';const wait=duplicate.$('ci-start').emit('click');await settle();await duplicate.$('ci-start').emit('click');assert.equal(duplicate.posts.length,1);duplicate.resolve('/api/tasks/1/track-ci');await wait;
  // First definitive key conflict can be corrected; ambiguous calls never receive a new key.
  const conflict=await ready();conflict.postMode='409';conflict.failureCode='idempotency_conflict';await conflict.$('ci-start').emit('click');const old=conflict.posts[0].body.key;conflict.postMode='success';await conflict.$('ci-read').emit('click');await conflict.$('ci-start').emit('click');assert.notEqual(conflict.posts[1].body.key,old);
  for(const patch of [{policy_digest:'e'.repeat(64)},{workflow_id:99}]){const changed=await ready();changed.postMode='abort';await changed.$('ci-start').emit('click');Object.assign(changed.ci.policies[0],patch);if(patch.workflow_id)changed.ci.policies[0].policy_digest='e'.repeat(64);await changed.$('ci-read').emit('click');assert(changed.$('ci-start').disabled);await changed.$('ci-start').emit('click');assert.equal(changed.posts.length,1);}
  const policyRejected=await ready();policyRejected.postMode='409';policyRejected.failureCode='ci_policy_changed';await policyRejected.$('ci-start').emit('click');const oldPolicyKey=policyRejected.posts[0].body.key;policyRejected.ci.policies[0].policy_digest='e'.repeat(64);policyRejected.postMode='abort';await policyRejected.$('ci-read').emit('click');assert(!policyRejected.$('ci-start').disabled);await policyRejected.$('ci-start').emit('click');assert.notEqual(policyRejected.posts[1].body.key,oldPolicyKey);assert.equal(policyRejected.posts[1].body.policy_digest,'e'.repeat(64));
  const staleControl=await ready();await staleControl.$('ci-start').emit('click');staleControl.postMode='abort';await control(staleControl,'stop').emit('click');const originalStop=clone(staleControl.posts.at(-1));staleControl.ci.tracks[0].revision++;await staleControl.$('ci-read').emit('click');staleControl.postMode='409';staleControl.failureCode='ci_stale_revision';await control(staleControl,'stop').emit('click');assert.deepEqual(staleControl.posts.at(-1),originalStop);assert.equal(buttons(staleControl).length,0);await staleControl.$('ci-read').emit('click');staleControl.postMode='success';await control(staleControl,'stop').emit('click');assert.equal(staleControl.posts.at(-1).body.expected_revision,2);
  const resumedLost=await ready();await resumedLost.$('ci-start').emit('click');await control(resumedLost,'stop').emit('click');resumedLost.postMode='lost_after_commit';await control(resumedLost,'resume').emit('click');const sent=resumedLost.posts.length;await resumedLost.$('ci-read').emit('click');assert.equal(resumedLost.posts.length,sent);assert(!control(resumedLost,'resume'));assert(control(resumedLost,'stop'));assert.match(resumedLost.$('ci-tracks').textContent,/观察窗口 2/);
  const source=await ready();source.postMode='abort';await source.$('ci-start').emit('click');source.ci.publication.head_sha='b'.repeat(40);await source.$('ci-read').emit('click');assert(source.$('ci-start').disabled);await source.$('ci-start').emit('click');assert.equal(source.posts.length,1);
  // Same track ID can never silently change its admitted HEAD, policy or deadline.
  const drift=await ready();await drift.$('ci-start').emit('click');drift.ci.tracks[0].workflow_id=99;await drift.$('ci-read').emit('click');assert.match(drift.$('ci-error').textContent,/固定来源/);assert.equal(buttons(drift).length,0);
  const numeric=await ready();await numeric.$('ci-start').emit('click');numeric.ci.tracks[0].observed_repository_id=101;numeric.ci.tracks[0].observed_pr_id=102;numeric.ci.tracks[0].observed_base_sha='b'.repeat(40);await numeric.$('ci-read').emit('click');assert.match(numeric.$('ci-tracks').textContent,/Repository ID：101/);numeric.ci.tracks[0].observed_repository_id=201;await numeric.$('ci-read').emit('click');assert.match(numeric.$('ci-error').textContent,/固定来源/);assert.equal(buttons(numeric).length,0);
  for(const mutate of [p=>p.publication.pr_url='javascript:alert(1)',p=>p.publication.head_sha='bad',p=>p.policies[0].app_id='15368',p=>p.policies[0].event='push',p=>p.policies[0].required_jobs=[],p=>p.policies[0].policy_digest='invalid']){const bad=await ready();mutate(bad.ci);await bad.$('ci-read').emit('click');assert(bad.$('ci-start').disabled);assert.match(bad.$('ci-error').textContent,/格式不匹配/);await bad.$('ci-start').emit('click');assert.equal(bad.posts.length,0);}
  const unavailable=await ready();await unavailable.$('ci-start').emit('click');unavailable.ci.policies=[];unavailable.ci.eligible=false;unavailable.ci.reason='No current policy';unavailable.ci.unavailable_policies=[{name:'required-ci',reason:'Observer executable unavailable; restore admitted configuration'}];await unavailable.$('ci-read').emit('click');assert.match(unavailable.$('ci-tracks').textContent,/正在跟踪/);assert.match(unavailable.$('ci-status').textContent,/restore admitted configuration/);assert(unavailable.$('ci-start').hidden);
  const safe=await ready(),attack='<img src=x onerror=alert(1)>';safe.ci.publication.base_branch=attack;safe.ci.policies[0].required_jobs=[attack];await safe.$('ci-read').emit('click');assert(safe.$('ci-scope').textContent.includes(attack));assert(safe.$('ci-policy-scope').textContent.includes(attack));assert.equal(safe.$('detail-ci').querySelectorAll('img').length,0);
  // Unknown observer state cannot be bypassed by a resume, elapsed deadline or a second start.
  const unknown=await ready();unknown.ci.tracks=[{...track('process_unknown'),diagnostic:{code:'ci_recovery_required',message:'Reconcile observer locally'}}];await unknown.$('ci-read').emit('click');assert.equal(buttons(unknown).length,0);assert(unknown.$('ci-start').hidden);assert.match(unknown.$('ci-tracks').textContent,/reconciliation/);await unknown.flush();assert.equal(unknown.posts.length,0);
  const lane=await ready();await lane.$('ci-start').emit('click');lane.ci.eligible=false;lane.ci.reason='Bound root changed; reconcile locally';lane.ci.lane_diagnostic={code:'ci_root_drift',message:'Bound root changed; reconcile locally'};await lane.$('ci-read').emit('click');assert.match(lane.$('ci-status').textContent,/Bound root changed/);assert.match(lane.$('ci-status').textContent,/ci_root_drift/);assert.match(lane.$('ci-tracks').textContent,/正在跟踪/);assert(control(lane,'stop').disabled);await control(lane,'stop').emit('click');assert.equal(lane.posts.length,1);
  const stopping=await ready();stopping.ci.tracks=[{...track(),stop_requested:true}];await stopping.$('ci-read').emit('click');assert.equal(buttons(stopping).length,0);assert.match(stopping.$('ci-tracks').textContent,/等待宿主/);
  const elapsed=await ready();elapsed.ci.tracks=[{...track('stopped'),deadline:Math.floor(Date.now()/1000)-1,window_started_at:Math.floor(Date.now()/1000)-3601}];await elapsed.$('ci-read').emit('click');assert(control(elapsed,'resume'));await control(elapsed,'resume').emit('click');assert.equal(elapsed.ci.tracks[0].window_generation,2);assert(elapsed.ci.tracks[0].deadline>Math.floor(Date.now()/1000));
  // Old DOM controls, late reads and POST responses cannot act on or replace another selected task.
  const stale=await ready();await stale.$('ci-start').emit('click');const oldStop=control(stale,'stop');await stale.select(2);await oldStop.emit('click');assert.equal(stale.posts.length,1);assert(stale.$('detail-ci').hidden);
  const late=await ready();late.delay=(_url,opts)=>opts.method==='POST';late.ignoreAbort=true;const response=late.$('ci-start').emit('click');await settle();await late.select(2);late.resolve('/api/tasks/1/track-ci');await response;assert.equal(late.$('detail-title').textContent,'任务 #2');assert(late.$('detail-ci').hidden);
  const racing=await ready();racing.delay=(url,opts)=>url.endsWith('/track-ci')&&opts.method==='POST';const racingStart=racing.$('ci-start').emit('click');await settle();racing.ci.tracks[0]={...racing.ci.tracks[0],revision:3,status:'configured_checks_passed'};await racing.$('ci-read').emit('click');racing.resolve('/api/tasks/1/track-ci');await racingStart;assert.match(racing.$('ci-tracks').textContent,/configured checks passed/);assert.match(racing.$('ci-tracks').textContent,/修订 3/);
  const stopRace=await ready();await stopRace.$('ci-start').emit('click');stopRace.delay=(url,opts)=>url.endsWith('/stop')&&opts.method==='POST';const stoppingResponse=control(stopRace,'stop').emit('click');await settle();stopRace.ci.tracks[0].revision=3;await stopRace.$('ci-read').emit('click');stopRace.resolve('/api/ci-tracks/10/stop');await stoppingResponse;assert.match(stopRace.$('ci-tracks').textContent,/修订 3/);assert(control(stopRace,'resume'));
  const oldRevision=await ready();await oldRevision.$('ci-start').emit('click');const oldSnapshot=clone(oldRevision.ci.tracks[0]);await control(oldRevision,'stop').emit('click');oldRevision.ci.tracks=[oldSnapshot];await oldRevision.$('ci-read').emit('click');assert.match(oldRevision.$('ci-error').textContent,/旧修订/);assert.equal(buttons(oldRevision).length,0);
  const lateRead=await ready();lateRead.delay=url=>url.endsWith('/ci-preview');lateRead.ignoreAbort=true;const read=lateRead.$('ci-read').emit('click');await settle();await lateRead.select(2);lateRead.resolve('/api/tasks/1/ci-preview');await read;assert(lateRead.$('detail-ci').hidden);
  for(const mode of ['bearer','session','hybrid']){const auth=await ready(mode);auth.delay=(url,opts)=>url.endsWith('/track-ci')&&opts.method==='POST';auth.ignoreAbort=true;const pending=auth.$('ci-start').emit('click');await settle();await auth.$('logout').emit('click');auth.resolve('/api/tasks/1/track-ci');await pending;assert(auth.$('detail-ci').hidden);assert.equal(auth.$('ci-scope').textContent,'');assert.equal(auth.posts.length,1);}
  const history=await ready();history.events.get('popstate')();assert(history.$('ci-start').disabled);await history.$('ci-start').emit('click');assert.equal(history.posts.length,0);await history.$('ci-read').emit('click');assert(!history.$('ci-start').disabled);
  console.log('PASS: CI tracking explicit policy start, pure local reads, fixed source/replay and revision controls, states/unknown gates, safe DOM, malformed sources, duplicate/auth/navigation/history/stale-response fences');
})().catch(error=>{console.error(error);process.exitCode=1});
