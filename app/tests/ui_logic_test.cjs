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
let db=[task(3),task(2,'claimed'),task(1,'finished','success')],status={active:task(2,'claimed'),recovery_required:false,diagnostic:null},requests=[],submissions=[],mode='success',pendingFetch=[],failList=false,delayList=false,delayDetail=false,delayPost=false;
async function fetch(url,opts){assert.equal(opts.headers.Authorization,'Bearer test-token');assert.equal(opts.redirect,'error');assert.equal(opts.credentials,'omit');requests.push({url,opts});let code=200,data;
 if(url==='/api/config')data={repositories:['repo'],agents:['agent'],tests:['test']};
 else if(url==='/api/status')data=status;
 else if(url==='/api/tasks'&&opts.method==='GET'){if(failList)throw new TypeError('offline');data=db;}
 else if(url==='/api/tasks'&&opts.method==='POST'){const body=JSON.parse(opts.body);submissions.push(body);assert.equal(body.job.publish,false);if(mode==='abort')throw new TypeError('offline');if(mode==='401'){code=401;data={error:'Unauthorized'}}else{data=db.find(t=>t.key===body.key)||{...task(Math.max(...db.map(t=>t.id))+1),key:body.key,payload:JSON.stringify(body.job)};db=[data,...db.filter(t=>t.id!==data.id)];}}
 else if(url.endsWith('/cancel'))data={requested:true};
 else data=db.find(t=>t.id===Number(url.split('/').at(-1)));
 const response={ok:code===200,status:code,json:async()=>JSON.parse(JSON.stringify(data))};
 if((delayDetail&&/\/tasks\/\d+$/.test(url))||(delayPost&&opts.method==='POST'&&url==='/api/tasks')||(delayList&&opts.method==='GET'&&url==='/api/tasks'))return await new Promise((resolve,reject)=>{pendingFetch.push({url,resolve:()=>resolve(response)});opts.signal.addEventListener('abort',()=>{const err=new Error('abort');err.name='AbortError';reject(err)})});
 return response;
}
const window={matchMedia:()=>({matches:false,addEventListener(){}}),addEventListener:(name,fn)=>windowEvents.set(name,fn)};
const ctx={console,document,window,navigator:{clipboard:{writeText:async()=>{}}},fetch,crypto:webcrypto,AbortController,TextEncoder,Uint8Array,setTimeout,clearTimeout,Date,Error,JSON,Array,String,Number,Boolean,Set,encodeURIComponent};
Object.defineProperties(ctx,{localStorage:{get(){throw Error('must not access localStorage')}},sessionStorage:{get(){throw Error('must not access sessionStorage')}}});
vm.runInNewContext(html.match(/<script>([\s\S]*?)<\/script>/)[1],ctx);
(async()=>{
 assert.equal(requests.length,0);$('token').value='test-token';await $('auth-form').emit('submit');assert($('auth-panel').hidden);assert.equal($('token').value,'');assert.equal($('task-list').children.length,3);
 // Repeated submission while a request is still pending produces exactly one POST.
 $('requirements').value='重复点击测试';delayPost=true;let first=$('task-form').emit('submit');await tick();await $('task-form').emit('submit');assert.equal(submissions.length,1);assert($('submit-task').disabled);delayPost=false;pendingFetch.find(x=>x.url==='/api/tasks').resolve();pendingFetch=[];await first;await tick();assert.equal($('detail-title').textContent,'任务 #4');assert.equal($('requirements').value,'');
 // Unknown network completion keeps immutable original key and job.
 $('requirements').value='网络重试测试';mode='abort';await $('task-form').emit('submit');await tick();assert.equal($('submit-task').textContent,'重试原提交');assert($('requirements').disabled);mode='success';await $('task-form').emit('submit');await tick();assert.deepEqual(submissions.at(-1),submissions.at(-2));assert.equal($('requirements').value,'');
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
 // Expired auth preserves unresolved submit and retries after reauthentication.
 $('requirements').value='令牌中断测试';mode='401';await $('task-form').emit('submit');assert(!$('auth-panel').hidden);assert.equal($('token').value,'');const authPending=submissions.at(-1);mode='success';$('token').value='test-token';await $('auth-form').emit('submit');assert.equal($('requirements').value,authPending.job.requirements);await $('task-form').emit('submit');await tick();assert.deepEqual(submissions.at(-1),authPending);
 // Logout cancels outstanding reads, clears data, and prevents stale updates.
 delayList=true;const refresh=$('refresh').emit('click');await tick();await $('logout').emit('click');for(const pending of pendingFetch)pending.resolve();pendingFetch=[];await refresh;await tick();assert(!$('auth-panel').hidden);assert($('task-detail').hidden);assert.equal($('requirements').value,'');assert.equal($('token').value,'');const after=requests.length;await new Promise(r=>setTimeout(r,2200));assert.equal(requests.length,after);
 console.log('PASS: 11 UI logic groups: no unauthenticated polling; token memory only; double-click submit; exact-key retry; 32-KiB UTF-8 validation; active-task merge; matched unknown diagnostic; selection race; network recovery; auth-expiry retry; logout abort/stale response/poll stop');
})().catch(e=>{console.error(e);process.exitCode=1}).finally(()=>{windowEvents.get('pagehide')()});
