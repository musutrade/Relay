// Run with: node app/tests/ui_logic_test.cjs
// Lightweight DOM fixture exercises actual index.html JavaScript without browser dependencies.
const assert=require('node:assert/strict'),fs=require('node:fs'),vm=require('node:vm'),{webcrypto}=require('node:crypto');
const root=require('node:path').resolve(__dirname,'../..'),html=fs.readFileSync(root+'/app/static/index.html','utf8');
const nodes=new Map(),windowEvents=new Map();let active=null;
class Element{
 constructor(tag='div',id=''){this.tagName=tag.toUpperCase();this.id=id;this.children=[];this.attrs={};this.dataset={};this.hidden=false;this.disabled=false;this._text='';this._value='';this.className='';this.events=new Map();this.open=false;this.classList={toggle:(name,on)=>{let set=new Set(this.className.split(' ').filter(Boolean));on?set.add(name):set.delete(name);this.className=[...set].join(' ')}};}
 get value(){return this._value}set value(v){this._value=String(v)}
 set innerHTML(_){throw Error('HTML must be rendered as text')}
 get textContent(){return this._text+this.children.map(x=>x.textContent||'').join('')}set textContent(v){this._text=String(v);this.children=[]}
 append(...items){for(const item of items){this.children.push(item);if(this.tagName==='SELECT'&&this.children.length===1)this.value=item.value;}}
 replaceChildren(...items){this.children=[];this._text='';this.append(...items)}
 setAttribute(k,v){this.attrs[k]=String(v)}getAttribute(k){return this.attrs[k]??null}
 addEventListener(k,f){let a=this.events.get(k)||[];a.push(f);this.events.set(k,a)}removeEventListener(){}
 focus(){active=this}showModal(){this.open=true}close(){this.open=false;this.emit('close')}
 querySelectorAll(tag){return this.children.flatMap(x=>[(x.tagName===tag.toUpperCase()?x:[]),...x.querySelectorAll(tag)]).flat()}
 emit(k,extra={}){return Promise.all((this.events.get(k)||[]).map(f=>f({preventDefault(){},...extra})))}
}
for(const m of html.matchAll(/<([a-z0-9]+)\b([^>]*\bid="([^"]+)"[^>]*)>/g)){let e=new Element(m[1],m[3]);e.hidden=/\bhidden\b/.test(m[2]);e.disabled=/\bdisabled\b/.test(m[2]);nodes.set(m[3],e)}
const filters=['all','queued','claimed','finished'].map(f=>{let e=new Element('button');e.dataset.filter=f;return e});
const document={getElementById:id=>{assert(nodes.has(id),'missing '+id);return nodes.get(id)},createElement:tag=>new Element(tag),querySelectorAll:()=>filters,documentElement:new Element('html'),get activeElement(){return active}};
const $=id=>nodes.get(id),tick=()=>new Promise(r=>setTimeout(r,0));
const task=(id,state='queued',outcome=null)=>({id,key:'key-'+id,payload:JSON.stringify({repository:'repo',agent:'agent',test:null,publish:false,requirements:'需求 '+id}),state,generation:state==='queued'?0:1,owner:state==='queued'?null:'host',result:outcome?JSON.stringify({outcome}):null});
// Fixtures model the server-owned decision; production UI never infers actions from results.
function operatorFixture(t){
 const result=JSON.parse(t.result||'null'),job=JSON.parse(t.payload),w=result?.workflow,last=w?.rounds?.at(-1),sha=w?.candidate_sha;
 const stopped=t.state==='finished'&&['failure','timed_out','cancelled'].includes(result?.outcome),reserved=Boolean(t.continuation_status),successor=t.continuation_status?.successor_id??null;
 const retry=stopped&&!successor&&(reserved||(Boolean(result.workspace)&&!result.draft_pr));
 const review=retry&&!reserved&&typeof job.workflow==='string'&&job.workflow===w?.name&&result.draft_pr===null&&[sha,w?.base_sha].every(v=>typeof v==='string'&&/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(v))&&w.reviewed_sha===null&&w.publication===null&&w.reconciliation_required===false&&last?.candidate_sha===sha&&last.review===null&&last.reviewer&&typeof last.reviewer==='object'&&!Array.isArray(last.reviewer)&&last.tests?.outcome==='success'&&last.tests.exit_code===0&&result.tests?.outcome==='success'&&result.tests.exit_code===0&&result.tests.signal===null&&result.tests.error===null;
 const action=id=>({id,quota_increase_allowed:false,quota_increase_required:false,min_quota_bytes:null,max_quota_bytes:104857600,requires_test_revalidation:id==='continue_review'});
 return {task_id:t.id,generation:t.generation,failure:null,resources:{usage:{logical_bytes:100,complete:true,measured_at:1700000000,reason:null},quota_bytes:104857600,host_policy_cap_bytes:104857600,snapshot_cap_bytes:104857600,enforcement:'logical_bytes_best_effort',os_hard_quota:false,disk_reserved:false},retained_result:{available:Boolean(t.result),immutable:true},workspace_retained:Boolean(result?.workspace),recovery:{inherited_quota_bytes:104857600,actions:successor?[]:[...(retry?[action('retry')]:[]),...(review?[action('continue_review')]:[])],blocked_reason:result?.draft_pr?'发布已尝试，需先核对外部结果后本机恢复':null,successor_id:successor,reserved_request:null}};
}
let config={repositories:['repo'],agents:['agent'],tests:['test']};
let db=[task(3),task(2,'claimed'),task(1,'finished','success')],status={active:task(2,'claimed'),recovery_required:false,diagnostic:null},requests=[],submissions=[],retries=[],reviewRetries=[],mode='success',pendingFetch=[],failList=false,delayList=false,delayDetail=false,delayPost=false,delayConfig=false,ignoreAbort=false;
const authMode=process.argv.includes('--session')?'session':process.argv.includes('--hybrid')?'hybrid':'bearer';
const excludedListIds=new Set();
let cookieAuthenticated=process.argv.includes('--restore'),rejectLogin=false,failLogout=false;
async function fetch(url,opts){assert.equal(opts.headers.Authorization,authMode==='bearer'&&url.startsWith('/api/')?'Bearer test-token':undefined);assert.equal(opts.redirect,'error');assert.equal(opts.credentials,authMode==='bearer'&&url.startsWith('/api/')?'omit':'same-origin');requests.push({url,opts});let code=200,data;
 if(url==='/auth/status')return {ok:true,status:200,json:async()=>({mode:authMode,authenticated:cookieAuthenticated})};
 if(url==='/auth/login'){assert.equal(opts.method,'POST');assert.deepEqual(JSON.parse(opts.body),{username:'operator',password:'exact password '});if(rejectLogin)return {ok:false,status:401,json:async()=>({error:'sensitive internal failure'})};cookieAuthenticated=true;return {ok:true,status:200,json:async()=>({authenticated:true})};}
 if(url==='/auth/logout'){assert.equal(opts.method,'POST');if(failLogout)throw new TypeError('offline');cookieAuthenticated=false;return {ok:true,status:200,json:async()=>({authenticated:false})};}

 if(url==='/api/config')data=config;
 else if(url==='/api/status')data=status;
 else if(url==='/api/tasks'&&opts.method==='GET'){if(failList)throw new TypeError('offline');data=db.filter(task=>!excludedListIds.has(task.id));}
 else if(url==='/api/tasks'&&opts.method==='POST'){const body=JSON.parse(opts.body);submissions.push(body);assert.equal(body.job.publish,false);if(mode==='abort')throw new TypeError('offline');if(mode==='401'||mode==='422'){code=Number(mode);data={error:mode==='401'?'Unauthorized':'Unknown workflow'}}else{data=db.find(t=>t.key===body.key)||{...task(Math.max(...db.map(t=>t.id))+1),key:body.key,payload:JSON.stringify(body.job)};db=[data,...db.filter(t=>t.id!==data.id)];}}
 else if(url.endsWith('/retry') || url.endsWith('/continue-review')) {
  const id=Number(url.split('/').at(-2)),body=JSON.parse(opts.body);(url.endsWith('/continue-review') ? reviewRetries : retries).push({id,...body});
  if(mode==='abort')throw new TypeError('offline');
  if(mode==='401'){code=401;data={error:'Unauthorized'}}else{
   data=db.find(t=>JSON.parse(t.payload).continuation?.predecessor_task_id===id);
   if(!data){const old=db.find(t=>t.id===id);data={...task(Math.max(...db.map(t=>t.id))+1),key:body.key,payload:JSON.stringify({...JSON.parse(old.payload),continuation:{workspace_task_id:id,predecessor_task_id:id,predecessor_generation:1,...(url.endsWith('/continue-review') ? {review_only:{base_sha:'b'.repeat(40),candidate_sha:'a'.repeat(40),round:0,...(body.review_focus ? {review_focus:body.review_focus} : {})}} : {})}})};}
   db.find(t=>t.id===id).continuation_status={successor_id:data.id};
   db=[data,...db.filter(t=>t.id!==data.id)];
  }
 }
 else if(url.endsWith('/operator'))data=operatorFixture(db.find(t=>t.id===Number(url.split('/').at(-2))));
 else if(url.endsWith('/cancel'))data={requested:true};
 else data=db.find(t=>t.id===Number(url.split('/').at(-1)));
 const response={ok:code===200,status:code,json:async()=>JSON.parse(JSON.stringify(data))};
 if((delayConfig&&url==='/api/config')||(delayDetail&&/\/tasks\/\d+$/.test(url))||(delayPost&&opts.method==='POST'&&(url==='/api/tasks'||url.endsWith('/retry')||url.endsWith('/continue-review')))||(delayList&&opts.method==='GET'&&url==='/api/tasks'))return await new Promise((resolve,reject)=>{pendingFetch.push({url,resolve:()=>resolve(response)});if(!ignoreAbort)opts.signal.addEventListener('abort',()=>{const err=new Error('abort');err.name='AbortError';reject(err)})});
 return response;
}
const window={matchMedia:()=>({matches:false,addEventListener(){}}),addEventListener:(name,fn)=>windowEvents.set(name,fn)};
const ctx={console,document,window,navigator:{clipboard:{writeText:async()=>{}}},fetch,crypto:webcrypto,AbortController,TextEncoder,Uint8Array,setTimeout,clearTimeout,Date,Error,JSON,Array,String,Number,Boolean,Set,encodeURIComponent};
Object.defineProperties(ctx,{localStorage:{get(){throw Error('must not access localStorage')}},sessionStorage:{get(){throw Error('must not access sessionStorage')}}});
vm.runInNewContext(html.match(/<script>([\s\S]*?)<\/script>/)[1],ctx);
(async()=>{
 await tick();
 if(authMode!=='bearer'){
  assert.equal(requests[0].url,'/auth/status');assert($('token').hidden);assert(!$('session-fields').hidden);
  const login=async()=>{$('username').value='operator';$('password').value='exact password ';await $('auth-form').emit('submit');assert.equal($('password').value,'')};
  if(cookieAuthenticated){assert($('auth-panel').hidden);assert.equal($('task-list').children.length,3);assert(!requests.some(r=>r.url==='/auth/login'));await $('logout').emit('click');}
  assert(!requests.some(r=>r.opts.headers.Authorization));
  rejectLogin=true;await login();assert(!$('auth-panel').hidden);assert.match($('auth-error').textContent,/登录未完成/);assert(!$('auth-error').textContent.includes('sensitive'));rejectLogin=false;
  await login();assert($('auth-panel').hidden);assert.equal($('task-list').children.length,3);
  failLogout=true;await $('logout').emit('click');assert($('auth-panel').hidden);assert.match($('logout-error').textContent,/未确认/);assert(cookieAuthenticated);assert(!$('logout').disabled);failLogout=false;
  // An expired cookie preserves the exact unresolved submission through a new login.
  $('requirements').value='会话过期重试';mode='401';await $('task-form').emit('submit');const original=submissions.at(-1);assert(!$('auth-panel').hidden);mode='success';await login();await $('task-form').emit('submit');assert.deepEqual(submissions.at(-1),original);
  // Refresh/back-forward cache restoration rechecks the cookie and restores access.
  windowEvents.get('pagehide')();windowEvents.get('pageshow')({persisted:true});await tick();assert($('auth-panel').hidden);assert.equal($('password').value,'');
  delayList=true;const stale=$('refresh').emit('click');await tick();await $('logout').emit('click');for(const pending of pendingFetch)pending.resolve();await stale;assert(!$('auth-panel').hidden);assert($('task-detail').hidden);assert(!cookieAuthenticated);assert.equal($('username').value,'');
  const after=requests.length;await new Promise(r=>setTimeout(r,2100));assert.equal(requests.length,after);
  console.log('PASS: '+authMode+' UI cookie restoration, login failure, password clearing, no bearer header/storage, logout revocation failure/success, pending retry, stale response fencing');return;
 }
 assert.equal(requests.length,1);$('token').value='test-token';await $('auth-form').emit('submit');assert($('auth-panel').hidden);assert.equal($('token').value,'');assert.equal($('task-list').children.length,3);assert($('workflow-field').hidden);
 // Legacy and empty workflow configs preserve the ordinary task form.
 await $('logout').emit('click');config.workflows=[];$('token').value='test-token';await $('auth-form').emit('submit');assert($('workflow-field').hidden);assert(!$('repository').disabled);
 await $('logout').emit('click');
 const reviewed={name:'reviewed',repository:'review-repo',developer:'native-dev',reviewer:'reviewer <img src=x onerror=alert(1)>',test:'full',max_repairs:2};
 config={repositories:['repo','review-repo'],agents:['agent','native-dev',reviewed.reviewer],tests:['test','full'],workflows:[reviewed,{...reviewed,name:'review-only',max_repairs:0}]};
 $('token').value='test-token';await $('auth-form').emit('submit');assert(!$('workflow-field').hidden);assert.equal($('workflow').value,'');
 const chooseWorkflow=async name=>{$('workflow').value=name;await $('workflow').emit('change')};
 $('test').value='test';await chooseWorkflow('reviewed');
 assert.deepEqual(['repository','agent','test'].map(id=>$(id).value),['review-repo','native-dev','full']);
 for(const id of ['repository','agent','test'])assert($(id).disabled);assert(!$('requirements').disabled);assert(!$('workflow').disabled);
 assert.match($('workflow-hint').textContent,/最多修复 2 轮/);assert.match($('workflow-hint').textContent,/<img/);assert.equal($('workflow-hint').children.length,0);
 await chooseWorkflow('review-only');assert.match($('workflow-hint').textContent,/最多修复 0 轮/);
 await chooseWorkflow('');assert.deepEqual(['repository','agent','test'].map(id=>$(id).value),['repo','agent','test']);for(const id of ['repository','agent','test'])assert(!$(id).disabled);assert($('workflow-hint').hidden);
 // Unknown names are rejected; disabled field tampering cannot override configured workflow values.
 $('requirements').value='配置约束';await chooseWorkflow('not-configured');await $('task-form').emit('submit');assert.equal(submissions.length,0);assert.match($('form-message').textContent,/允许的审查工作流/);
 await chooseWorkflow('reviewed');$('repository').value='repo';$('agent').value='agent';$('test').value='test';
 // Repeated submission while a request is still pending produces exactly one POST.
 $('requirements').value='重复点击测试';delayPost=true;let first=$('task-form').emit('submit');await tick();await $('task-form').emit('submit');assert.equal(submissions.length,1);assert.deepEqual(submissions[0].job,{repository:'review-repo',requirements:'重复点击测试',agent:'native-dev',test:'full',publish:false,workflow:'reviewed'});assert($('workflow').disabled);assert($('submit-task').disabled);delayPost=false;pendingFetch.find(x=>x.url==='/api/tasks').resolve();pendingFetch=[];await first;await tick();assert.equal($('detail-title').textContent,'任务 #4');assert.equal($('requirements').value,'');assert.match($('detail-meta').textContent,/审查工作流reviewed/);await chooseWorkflow('');
 // Unknown network completion keeps immutable original key and job.
 $('requirements').value='网络重试测试';mode='abort';await $('task-form').emit('submit');await tick();assert.equal($('submit-task').textContent,'重试原提交');assert($('requirements').disabled);mode='success';await $('task-form').emit('submit');await tick();assert.deepEqual(submissions.at(-1),submissions.at(-2));assert(!Object.hasOwn(submissions.at(-1).job,'workflow'));assert.equal($('requirements').value,'');
 // Byte-oriented host bound, not JS character length.
 const count=submissions.length;$('requirements').value='界'.repeat(11000);await $('task-form').emit('submit');assert.equal(submissions.length,count);assert.match($('form-message').textContent,/32 KiB/);$('requirements').value='';
 // Older active claim is retained even if absent from the recent list.
 db=[task(110),task(109)];status={active:task(2,'claimed'),recovery_required:true,diagnostic:JSON.stringify({outcome:'unknown',reason:'<script>never execute</script>'})};await $('refresh').emit('click');await tick();assert.equal($('task-list').children.length,3);assert.equal($('claimed-count').textContent,'1');assert(!$('recovery-banner').hidden);assert($('submit-task').disabled);
 const buttons=()=>$('task-list').querySelectorAll('button');const select=id=>buttons().find(e=>e.dataset.taskId===String(id)).emit('click');
 // Fixture detail endpoint must contain active too.
 db.push(status.active);await select(2);assert.equal($('detail-title').textContent,'任务 #2');assert(!$('detail-warning').hidden);assert(!$('detail-diagnostic').hidden);assert.match($('detail-diagnostic-text').textContent,/<script>/);await select(110);assert($('detail-diagnostic').hidden);
 // Usage comes from persisted stage.provider.usage, never arbitrary nested data or log text.
 const usageTask=db.find(t=>t.id===110),originalUsageTask={state:usageTask.state,result:usageTask.result};usageTask.state='finished';
 const providerResult=(usage,provider='codex_app_server')=>({provider,summary:'provider summary',usage});
 const showUsage=async result=>{usageTask.result=JSON.stringify(result);await select(110)};
 const showProvider=async provider=>showUsage({outcome:'success',agent:{outcome:'success',provider},tests:null,draft_pr:null});
 const usageStage=()=>$('usage-stages').children[0],counts=node=>node.children.map(group=>group.children[1].textContent),primaryCounts=()=>counts(usageStage().children[2]);
 const lastSnapshot=()=>usageStage().children.find(child=>child.className==='usage-snapshot'),unknownCounts=['未知','未知','未知','未知'];
 const snapshotUsage={input_tokens:100,cached_input_tokens:80,output_tokens:10,reasoning_output_tokens:4};
 await showProvider(providerResult({...snapshotUsage,usage_scope:'last_snapshot',turn_total:{input_tokens:350,cached_input_tokens:240,output_tokens:35,reasoning_output_tokens:15}}));
 assert(!$('detail-usage').hidden);assert.equal($('usage-stages').children.length,1);assert.match(usageStage().children[1].textContent,/本轮累计/);assert.deepEqual(primaryCounts(),['350','35','240','15']);
 assert.match(lastSnapshot().children[0].textContent,/最后一次快照/);assert.deepEqual(counts(lastSnapshot().children[1]),['100','10','80','4']);assert(!usageStage().textContent.includes('供应商报告费用'));
 assert.equal($('detail-result').textContent,JSON.stringify(JSON.parse(usageTask.result),null,2));
 // No turn baseline means unknown, even when a last snapshot exists. Partial fields stay unknown.
 await showProvider(providerResult({...snapshotUsage,usage_scope:'last_snapshot'}));assert.deepEqual(primaryCounts(),unknownCounts);assert.deepEqual(counts(lastSnapshot().children[1]),['100','10','80','4']);
 await showProvider(providerResult({...snapshotUsage,usage_scope:'last_snapshot',turn_total:null}));assert.deepEqual(primaryCounts(),unknownCounts);
 await showProvider(providerResult({usage_scope:'last_snapshot',turn_total:{input_tokens:0,output_tokens:7,cached_input_tokens:null}}));assert.deepEqual(primaryCounts(),['0','7','未知','未知']);assert.deepEqual(counts(lastSnapshot().children[1]),unknownCounts);
 // Actual zeros are retained, including optional reported cost and provider counters.
 const zeros={input_tokens:0,output_tokens:0,cached_input_tokens:0,reasoning_output_tokens:0};
 await showProvider(providerResult({...zeros,usage_scope:'last_snapshot',turn_total:zeros,total_cost_usd:0,num_turns:0,cache_creation_input_tokens:0}));
 assert.deepEqual(primaryCounts(),['0','0','0','0']);assert.deepEqual(counts(lastSnapshot().children[1]),['0','0','0','0']);assert.match(usageStage().textContent,/供应商报告费用：0 USD（非实际账单/);assert.match(usageStage().textContent,/缓存写入 token：0/);assert.match(usageStage().textContent,/turn 数：0/);
 // Legacy app-server results cannot be silently relabeled as verified turn totals.
 await showProvider(providerResult(snapshotUsage));assert.match(usageStage().children[1].textContent,/供应商报告的 token（统计范围未确认）/);assert.deepEqual(primaryCounts(),['100','10','80','4']);assert.equal(lastSnapshot(),undefined);assert(!usageStage().textContent.includes('本轮累计'));
 // Other providers retain their own semantics: Claude input excludes cached/write tokens.
 await showProvider(providerResult({input_tokens:60,cached_input_tokens:40,cache_creation_input_tokens:100,output_tokens:12,total_cost_usd:0.012345},'claude_cli'));
 assert.deepEqual(primaryCounts(),['60','12','40','未知']);assert.match(usageStage().textContent,/统计范围未确认/);assert.match(usageStage().textContent,/缓存写入 token：100/);assert.match(usageStage().textContent,/供应商报告费用：0.012345 USD/);assert(!usageStage().textContent.includes('包含于'));
 await showProvider(providerResult({...snapshotUsage,usage_scope:'future_scope',turn_total:zeros},'codex_cli'));assert.deepEqual(primaryCounts(),['100','10','80','4']);assert.equal(lastSnapshot(),undefined);assert.match(usageStage().textContent,/统计范围未确认/);
 // Missing, malformed, unsafe or negative counts never coerce to zero or an invented total.
 for(const usage of [undefined,null,{},[],{input_tokens:'0',output_tokens:-1,cached_input_tokens:1.5,reasoning_output_tokens:Number.MAX_SAFE_INTEGER+1,total_cost_usd:'0'}]){await showProvider(providerResult(usage));assert.deepEqual(primaryCounts(),unknownCounts);assert(!usageStage().textContent.includes('供应商报告费用'))}
 // Names stay text, numerical fields reject injected strings, and only known stages are inspected.
 const attack='<img src=x onerror=alert(1)>';
 await showUsage({outcome:'success',agent:{provider:providerResult({input_tokens:attack,total_cost_usd:attack},attack),stdout:JSON.stringify({provider:providerResult(snapshotUsage)})},tests:{provider:providerResult(zeros,'test-provider')},draft_pr:{provider:providerResult(snapshotUsage,'publish-provider')},workflow:{rounds:[{developer:{provider:providerResult(snapshotUsage)}}]}});
 assert.equal($('usage-stages').children.length,3);assert.match(usageStage().children[0].textContent,/<img/);assert.equal(usageStage().children[0].children.length,0);assert.deepEqual(primaryCounts(),unknownCounts);assert.equal($('usage-stages').querySelectorAll('img').length,0);assert.match($('usage-stages').children[1].children[0].textContent,/测试阶段/);assert.match($('usage-stages').children[2].children[0].textContent,/发布阶段/);
 for(const result of [{outcome:'success',agent:null},{outcome:'success',provider:providerResult(snapshotUsage),agent:{stdout:JSON.stringify({provider:providerResult(snapshotUsage)})}},{agent:{provider:[]}},[],null,'plain result']){await showUsage(result);assert($('detail-usage').hidden);assert.equal($('usage-stages').children.length,0)}
 Object.assign(usageTask,originalUsageTask);await select(110);assert($('detail-usage').hidden);assert.equal($('usage-stages').children.length,0);
 // A stale detail response cannot replace a newer selection.
 status={active:null,recovery_required:false,diagnostic:null};await $('refresh').emit('click');await tick();delayDetail=true;const oldSelection=select(109);await tick();const newSelection=select(110);await tick();const responses=pendingFetch;pendingFetch=[];responses.find(x=>x.url.endsWith('/110')).resolve();await tick();responses.find(x=>x.url.endsWith('/109')).resolve();await tick();await Promise.all([oldSelection,newSelection]);assert.equal($('detail-title').textContent,'任务 #110');delayDetail=false;
 // Network failure retains prior task list and recovers on successful sync.
 failList=true;await $('refresh').emit('click');await tick();assert(!$('network-banner').hidden);assert.equal(buttons().length,3);failList=false;await $('refresh-error').emit('click');await tick();assert($('network-banner').hidden);
 // Preserved-work continuation is explicit, idempotent after ambiguous network completion, and double-click guarded.
 const failed={...task(120,'finished','failure'),result:JSON.stringify({outcome:'failure',workspace:'/private/task-120',draft_pr:null})};db=[failed,...db];await $('refresh').emit('click');await select(120);assert(!$('retry-task').hidden);assert(!$('retry-task').disabled);
 await $('retry-task').emit('click');assert($('retry-dialog').open);await $('retry-dismiss').emit('click');assert.equal(retries.length,0);
 mode='abort';await $('retry-task').emit('click');await $('retry-confirm').emit('click');assert.equal(retries.length,1);const originalRetry=retries[0];assert.match($('detail-error').textContent,/续接未确认/);
 mode='success';delayPost=true;await $('retry-task').emit('click');const continuing=$('retry-confirm').emit('click');await tick();await $('retry-confirm').emit('click');assert.equal(retries.length,2);assert.deepEqual(retries[1],originalRetry);assert($('retry-task').disabled);delayPost=false;pendingFetch.find(x=>x.url.endsWith('/retry')).resolve();pendingFetch=[];await continuing;await tick();assert.equal($('detail-title').textContent,'任务 #121');assert.match($('detail-meta').textContent,/续接自#120/);
 // Revisit and refresh read persisted successor metadata; no second action remains.
 await select(120);assert($('retry-task').hidden);assert($('retry-task').disabled);assert(!$('continuation-next').hidden);assert.match($('continuation-next').textContent,/#121/);
 const retryCount=retries.length;await $('retry-task').emit('click');assert(!$('retry-dialog').open);assert.equal(retries.length,retryCount);
 await $('refresh').emit('click');await select(120);assert($('retry-task').hidden);await $('continuation-next').emit('click');assert.equal($('detail-title').textContent,'任务 #121');
 // Off-page child navigation survives an intervening poll and reselecting its row.
 excludedListIds.add(121);await select(120);await $('refresh').emit('click');delayDetail=true;
 const offPage=$('continuation-next').emit('click');await tick();await $('refresh').emit('click');
 delayDetail=false;pendingFetch.find(x=>x.url==='/api/tasks/121').resolve();pendingFetch=[];await offPage;assert.equal($('detail-title').textContent,'任务 #121');
 await select(121);await $('refresh').emit('click');assert.equal($('detail-title').textContent,'任务 #121');
 await select(120);await $('continuation-next').emit('click');await $('refresh').emit('click');assert.equal($('detail-title').textContent,'任务 #121');await select(120);await filters.find(node=>node.dataset.filter==='queued').emit('click');await $('refresh').emit('click');assert.equal($('detail-title').textContent,'任务 #121');await filters.find(node=>node.dataset.filter==='all').emit('click');excludedListIds.clear();
 // A persisted reservation is distinguishable and can recover the same submission.
 failed.continuation_status={successor_id:null};await select(120);assert(!$('retry-task').hidden);assert(!$('retry-task').disabled);assert.equal($('retry-task').textContent,'恢复已预留的续接');
 await $('retry-task').emit('click');failed.continuation_status={successor_id:121};await $('refresh').emit('click');await $('retry-confirm').emit('click');assert.equal(retries.length,retryCount);assert.equal($('detail-title').textContent,'任务 #121');
 // Reauthentication reconstructs state from the server, not browser storage.
 await $('logout').emit('click');$('token').value='test-token';await $('auth-form').emit('submit');await select(120);assert($('retry-task').hidden);assert.match($('continuation-next').textContent,/#121/);
 failed.continuation_status=null;
 failed.result=JSON.stringify({outcome:'failure',workspace:'/private/task-120',draft_pr:{outcome:'failure'}});await select(120);assert($('retry-task').disabled);assert.match($('cancel-note').textContent,/发布已尝试/);
 // Review-only eligibility is based on the host's last exact-candidate evidence.
 const sha='a'.repeat(40),reviewResult=()=>({outcome:'failure',workspace:'/private/task-140',draft_pr:null,tests:{outcome:'success',exit_code:0,signal:null,error:null},workflow:{name:'reviewed',base_sha:'b'.repeat(40),candidate_sha:sha,reviewed_sha:null,publication:null,reconciliation_required:false,rounds:[{candidate_sha:sha,tests:{outcome:'success',exit_code:0},review:null,reviewer:{outcome:'failure'}}]}});
 const reviewFailed={...task(140,'finished','failure'),payload:JSON.stringify({...JSON.parse(task(140).payload),workflow:'reviewed'}),result:JSON.stringify(reviewResult())};db=[reviewFailed,...db];await $('refresh').emit('click');await select(140);
 assert(!$('review-task').hidden);assert(!$('review-task').disabled);assert(!$('retry-task').hidden);
 for(const change of [r=>r.outcome='success',r=>r.outcome='unknown',r=>r.workspace=null,r=>r.draft_pr={},r=>r.tests=null,r=>r.tests.outcome='failure',r=>r.tests.exit_code=1,r=>r.tests.signal=9,r=>r.tests.error='error',r=>r.workflow.base_sha=null,r=>r.workflow.name='other',r=>r.workflow.candidate_sha='not a sha',r=>r.workflow.reviewed_sha=sha,r=>r.workflow.publication={},r=>r.workflow.reconciliation_required=true,r=>r.workflow.rounds[0].candidate_sha='b'.repeat(40),r=>r.workflow.rounds[0].review={},r=>r.workflow.rounds[0].reviewer=null,r=>r.workflow.rounds[0].reviewer=[],r=>r.workflow.rounds[0].tests.outcome='failure',r=>r.workflow.rounds[0].tests.exit_code=1,r=>r.workflow.rounds.push({candidate_sha:sha,tests:{outcome:'failure'},review:null,reviewer:null})]) {
  const value=reviewResult();change(value);reviewFailed.result=JSON.stringify(value);await select(140);assert($('review-task').hidden,JSON.stringify(value));await $('review-task').emit('click');assert(!$('retry-dialog').open);
 }
 reviewFailed.result=JSON.stringify(reviewResult());reviewFailed.state='claimed';await select(140);assert($('review-task').hidden);reviewFailed.state='finished';await select(140);
 // Publication remains the original explicit job choice and is disclosed.
 const originalReviewPayload=reviewFailed.payload;reviewFailed.payload=JSON.stringify({...JSON.parse(originalReviewPayload),publish:true});await select(140);await $('review-task').emit('click');assert.match($('retry-dialog-description').textContent,/审查批准后仍按原设置发布/);await $('retry-dismiss').emit('click');reviewFailed.payload=originalReviewPayload;await select(140);
 // Confirmation is optional-focus, bounded in UTF-8 bytes and cancel-safe.
 await $('review-task').emit('click');assert($('retry-dialog').open);assert(!$('review-focus-field').hidden);assert.match($('retry-dialog-description').textContent,/重新运行配置的测试一次/);assert.match($('retry-dialog-description').textContent,/不调用开发者/);$('review-focus').value='discard';await $('retry-dismiss').emit('click');assert.equal(reviewRetries.length,0);assert.equal($('review-focus').value,'');
 await $('review-task').emit('click');for(const focus of ['   ','界'.repeat(2731)]){$('review-focus').value=focus;await $('retry-confirm').emit('click');assert($('retry-dialog').open);assert.match($('retry-dialog-error').textContent,/8192 UTF-8/);assert.equal(reviewRetries.length,0)}
 const reviewFocus='界'.repeat(2730)+'ab';$('review-focus').value=reviewFocus;mode='abort';await $('retry-confirm').emit('click');assert.equal(reviewRetries.length,1);assert.equal(reviewRetries[0].review_focus,reviewFocus);assert.equal(reviewRetries[0].revalidate_tests,true);assert.equal(reviewRetries[0].confirm_stopped_and_reconciled,true);assert($('retry-task').disabled);const originalReview=reviewRetries[0];
 // Unknown completion locks both mode and payload; authentication interruption retains it.
 await $('review-task').emit('click');assert($('review-focus').disabled);assert.equal($('review-focus').value,reviewFocus);$('review-focus').value='cannot alter pending request';mode='401';await $('retry-confirm').emit('click');assert.deepEqual(reviewRetries.at(-1),originalReview);assert(!$('auth-panel').hidden);assert.equal($('review-focus').value,'');
 mode='success';$('token').value='test-token';await $('auth-form').emit('submit');await select(140);await $('review-task').emit('click');assert.equal($('review-focus').value,reviewFocus);assert($('review-focus').disabled);
 delayPost=true;const reviewContinuing=$('retry-confirm').emit('click');await tick();await $('retry-confirm').emit('click');await $('review-task').emit('click');assert.equal(reviewRetries.length,3);assert.deepEqual(reviewRetries.at(-1),originalReview);assert($('review-task').disabled);delayPost=false;pendingFetch.find(x=>x.url.endsWith('/continue-review')).resolve();pendingFetch=[];await reviewContinuing;await tick();assert.equal($('detail-title').textContent,'任务 #141');assert.match($('detail-meta').textContent,/仅审查（先复验测试）/);
 await select(140);assert($('review-task').hidden);assert($('retry-task').hidden);assert.match($('continuation-next').textContent,/#141/);await $('review-task').emit('click');assert(!$('retry-dialog').open);
 // A fresh empty focus explicitly retains the original and navigation fences stale dialogs.
 const secondReview={...reviewFailed,id:145,key:'review-145',continuation_status:null};db=[secondReview,...db];await $('refresh').emit('click');await select(145);await $('review-task').emit('click');assert.equal($('review-focus').value,'');assert(!$('review-focus').disabled);await $('retry-confirm').emit('click');await tick();assert.equal(reviewRetries.at(-1).review_focus,null);assert.equal($('detail-title').textContent,'任务 #146');
 // Expired auth preserves unresolved workflow submission even if the new config removes its profiles.
 await chooseWorkflow('reviewed');$('requirements').value='令牌中断测试';mode='401';await $('task-form').emit('submit');assert(!$('auth-panel').hidden);assert.equal($('token').value,'');const authPending=submissions.at(-1);assert.equal(authPending.job.workflow,'reviewed');assert($('workflow-field').hidden);assert.equal($('workflow').value,'');assert.equal($('workflow-hint').textContent,'');config={repositories:['repo'],agents:['agent'],tests:['test'],workflows:[]};mode='success';$('token').value='test-token';await $('auth-form').emit('submit');assert.equal($('requirements').value,authPending.job.requirements);assert.equal($('workflow').value,'reviewed');assert.equal($('agent').value,'native-dev');assert(!$('workflow-field').hidden);assert($('workflow').disabled);assert.match($('workflow-hint').textContent,/相同工作流与参数/);await $('task-form').emit('submit');await tick();assert.deepEqual(submissions.at(-1),authPending);assert.equal($('workflow').value,'');assert($('workflow-field').hidden);assert.equal($('agent').value,'agent');assert(!$('agent').disabled);
 // Logout cancels outstanding reads, clears data, and prevents stale updates.
 delayList=true;const refresh=$('refresh').emit('click');await tick();await $('logout').emit('click');for(const pending of pendingFetch)pending.resolve();pendingFetch=[];await refresh;await tick();assert(!$('auth-panel').hidden);assert($('task-detail').hidden);assert.equal($('requirements').value,'');assert.equal($('token').value,'');assert.equal($('workflow').value,'');assert($('workflow-field').hidden);assert.equal($('workflow-hint').textContent,'');const after=requests.length;await new Promise(r=>setTimeout(r,2200));assert.equal(requests.length,after);
 // Back/forward cache restoration clears workflow choices, locks fields, and drops the session.
 delayList=false;config={repositories:['repo','review-repo'],agents:['agent','native-dev',reviewed.reviewer],tests:['test','full'],workflows:[reviewed]};$('token').value='test-token';await $('auth-form').emit('submit');await chooseWorkflow('reviewed');$('requirements').value='返回页面前的需求';windowEvents.get('pagehide')();windowEvents.get('pageshow')({persisted:true});assert(!$('auth-panel').hidden);assert.equal($('workflow').value,'');assert($('workflow-field').hidden);assert($('workflow').disabled);assert.equal($('requirements').value,'');
 // A stale successful config response cannot restore an expired session or workflow selection.
 delayConfig=true;ignoreAbort=true;$('token').value='test-token';const staleConnect=$('auth-form').emit('submit');await tick();windowEvents.get('pageshow')({persisted:true});for(const pending of pendingFetch)pending.resolve();pendingFetch=[];await staleConnect;assert(!$('auth-panel').hidden);assert.equal($('workflow').value,'');assert($('workflow-field').hidden);assert($('repository').disabled);delayConfig=false;ignoreAbort=false;
 // Rejection of a formerly configured workflow returns to an editable ordinary form.
 $('token').value='test-token';await $('auth-form').emit('submit');await chooseWorkflow('reviewed');$('requirements').value='被移除的工作流';mode='401';await $('task-form').emit('submit');config={repositories:[],agents:[],tests:[],workflows:[]};mode='422';$('token').value='test-token';await $('auth-form').emit('submit');const rejectedOriginal=submissions.at(-1);assert(!$('submit-task').disabled);await $('task-form').emit('submit');await tick();assert.deepEqual(submissions.at(-1),rejectedOriginal);assert.equal($('workflow').value,'');assert($('workflow-field').hidden);assert(!$('requirements').disabled);assert(!$('repository').disabled);assert.equal($('requirements').value,'被移除的工作流');assert.match($('form-message').textContent,/提交被拒绝/);assert($('submit-task').disabled);
 for(const args of [['--session'],['--session','--restore'],['--hybrid'],['--hybrid','--restore']]){const result=require('node:child_process').spawnSync(process.execPath,[__filename,...args],{stdio:'inherit'});assert.equal(result.status,0,'session UI subprocess failed')}
 const resourceTests=require('node:child_process').spawnSync(process.execPath,[require('node:path').join(__dirname,'ui_resources_test.cjs')],{stdio:'inherit'});assert.equal(resourceTests.status,0,'resource UI subprocess failed');
 const capabilityTests=require('node:child_process').spawnSync(process.execPath,[require('node:path').join(__dirname,'ui_capabilities_test.cjs')],{stdio:'inherit'});assert.equal(capabilityTests.status,0,'capability UI subprocess failed');
 const roleTests=require('node:child_process').spawnSync(process.execPath,[require('node:path').join(__dirname,'ui_roles_test.cjs')],{stdio:'inherit'});assert.equal(roleTests.status,0,'role UI subprocess failed');
 const replacementTests=require('node:child_process').spawnSync(process.execPath,[require('node:path').join(__dirname,'ui_replacement_test.cjs')],{stdio:'inherit'});assert.equal(replacementTests.status,0,'replacement UI subprocess failed');
 console.log('PASS: UI logic groups: review-only last-candidate eligibility; UTF-8 focus validation; immutable review retry through auth expiry; duplicate review guards; shared successor navigation; per-stage token usage; verified turn totals and separate snapshots; legacy unverified scope; missing/zero/malformed counts; provider cache semantics; optional reported cost; safe usage text; stale config response; rejected workflow reset; optional workflow config; locked metadata and ordinary restoration; invalid selection; safe workflow text; workflow payload; workflow auth retry after config changes; back/forward session reset; no unauthenticated polling; token memory only; double-click submit; exact-key retry; 32-KiB UTF-8 validation; active-task merge; matched unknown diagnostic; selection race; network recovery; auth-expiry retry; logout abort/stale response/poll stop');
})().catch(e=>{console.error(e);process.exitCode=1}).finally(()=>{windowEvents.get('pagehide')()});
