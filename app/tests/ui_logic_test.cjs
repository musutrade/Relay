// Run with: node app/tests/ui_logic_test.cjs
// Lightweight DOM fixture exercises actual index.html JavaScript without browser dependencies.
const assert=require('node:assert/strict'),fs=require('node:fs'),vm=require('node:vm'),{webcrypto}=require('node:crypto');
const root=require('node:path').resolve(__dirname,'../..'),html=fs.readFileSync(root+'/app/static/index.html','utf8');
const nodes=new Map(),windowEvents=new Map();let active=null;
class Element{
 constructor(tag='div',id=''){this.tagName=tag.toUpperCase();this.id=id;this.children=[];this.attrs={};this.dataset={};this.hidden=false;this.disabled=false;this._text='';this._value='';this.className='';this.events=new Map();this.open=false;this.classList={toggle:(name,on)=>{let set=new Set(this.className.split(' ').filter(Boolean));on?set.add(name):set.delete(name);this.className=[...set].join(' ')}};}
 get value(){return this._value}set value(v){this._value=String(v)}
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
let config={repositories:['repo'],agents:['agent'],tests:['test']};
let db=[task(3),task(2,'claimed'),task(1,'finished','success')],status={active:task(2,'claimed'),recovery_required:false,diagnostic:null},requests=[],submissions=[],mode='success',pendingFetch=[],failList=false,delayList=false,delayDetail=false,delayPost=false,delayConfig=false,ignoreAbort=false;
const authMode=process.argv.includes('--session')?'session':process.argv.includes('--hybrid')?'hybrid':'bearer';
let cookieAuthenticated=process.argv.includes('--restore'),rejectLogin=false,failLogout=false;
async function fetch(url,opts){assert.equal(opts.headers.Authorization,authMode==='bearer'&&url.startsWith('/api/')?'Bearer test-token':undefined);assert.equal(opts.redirect,'error');assert.equal(opts.credentials,authMode==='bearer'&&url.startsWith('/api/')?'omit':'same-origin');requests.push({url,opts});let code=200,data;
 if(url==='/auth/status')return {ok:true,status:200,json:async()=>({mode:authMode,authenticated:cookieAuthenticated})};
 if(url==='/auth/login'){assert.equal(opts.method,'POST');assert.deepEqual(JSON.parse(opts.body),{username:'operator',password:'exact password '});if(rejectLogin)return {ok:false,status:401,json:async()=>({error:'sensitive internal failure'})};cookieAuthenticated=true;return {ok:true,status:200,json:async()=>({authenticated:true})};}
 if(url==='/auth/logout'){assert.equal(opts.method,'POST');if(failLogout)throw new TypeError('offline');cookieAuthenticated=false;return {ok:true,status:200,json:async()=>({authenticated:false})};}

 if(url==='/api/config')data=config;
 else if(url==='/api/status')data=status;
 else if(url==='/api/tasks'&&opts.method==='GET'){if(failList)throw new TypeError('offline');data=db;}
 else if(url==='/api/tasks'&&opts.method==='POST'){const body=JSON.parse(opts.body);submissions.push(body);assert.equal(body.job.publish,false);if(mode==='abort')throw new TypeError('offline');if(mode==='401'||mode==='422'){code=Number(mode);data={error:mode==='401'?'Unauthorized':'Unknown workflow'}}else{data=db.find(t=>t.key===body.key)||{...task(Math.max(...db.map(t=>t.id))+1),key:body.key,payload:JSON.stringify(body.job)};db=[data,...db.filter(t=>t.id!==data.id)];}}
 else if(url.endsWith('/cancel'))data={requested:true};
 else data=db.find(t=>t.id===Number(url.split('/').at(-1)));
 const response={ok:code===200,status:code,json:async()=>JSON.parse(JSON.stringify(data))};
 if((delayConfig&&url==='/api/config')||(delayDetail&&/\/tasks\/\d+$/.test(url))||(delayPost&&opts.method==='POST'&&url==='/api/tasks')||(delayList&&opts.method==='GET'&&url==='/api/tasks'))return await new Promise((resolve,reject)=>{pendingFetch.push({url,resolve:()=>resolve(response)});if(!ignoreAbort)opts.signal.addEventListener('abort',()=>{const err=new Error('abort');err.name='AbortError';reject(err)})});
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
 // A stale detail response cannot replace a newer selection.
 status={active:null,recovery_required:false,diagnostic:null};await $('refresh').emit('click');await tick();delayDetail=true;const oldSelection=select(109);await tick();const newSelection=select(110);await tick();const responses=pendingFetch;pendingFetch=[];responses.find(x=>x.url.endsWith('/110')).resolve();await tick();responses.find(x=>x.url.endsWith('/109')).resolve();await tick();await Promise.all([oldSelection,newSelection]);assert.equal($('detail-title').textContent,'任务 #110');delayDetail=false;
 // Network failure retains prior task list and recovers on successful sync.
 failList=true;await $('refresh').emit('click');await tick();assert(!$('network-banner').hidden);assert.equal(buttons().length,3);failList=false;await $('refresh-error').emit('click');await tick();assert($('network-banner').hidden);
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
 console.log('PASS: UI logic groups: stale config response; rejected workflow reset; optional workflow config; locked metadata and ordinary restoration; invalid selection; safe workflow text; workflow payload; workflow auth retry after config changes; back/forward session reset; no unauthenticated polling; token memory only; double-click submit; exact-key retry; 32-KiB UTF-8 validation; active-task merge; matched unknown diagnostic; selection race; network recovery; auth-expiry retry; logout abort/stale response/poll stop');
})().catch(e=>{console.error(e);process.exitCode=1}).finally(()=>{windowEvents.get('pagehide')()});
