"""Optional real-browser smoke test: python3 app/tests/ui_smoke.py.
Requires Python Playwright and an installed Chromium (or CHROMIUM_PATH).
Uses a local fixture, fake token, and no real repository execution.
The application itself has no JavaScript/build dependencies.
"""
import json, threading, tempfile, os
from pathlib import Path
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from playwright.sync_api import sync_playwright, expect
HTML = (Path(__file__).resolve().parents[1] / 'static' / 'index.html').read_bytes()
SCREENSHOTS = Path(os.environ.get('RELAY_UI_SCREENSHOTS') or tempfile.mkdtemp(prefix='relay-ui-smoke-'))
SCREENSHOTS.mkdir(parents=True, exist_ok=True)
print(f'Screenshots: {SCREENSHOTS}')
class Server(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.send_header('Content-Type','text/html; charset=utf-8'); self.end_headers(); self.wfile.write(HTML)
    def log_message(self,*a): pass
server=ThreadingHTTPServer(('127.0.0.1',0),Server)
threading.Thread(target=server.serve_forever,daemon=True).start()
base=f'http://127.0.0.1:{server.server_port}'
def task(i, state='queued', requirement=None, outcome=None):
    return {'id':i,'key':f'key-{i}','payload':json.dumps({'repository':'relay-demo','requirements':requirement or f'改进工作台的任务体验 #{i}','agent':'codex','test':'unit','publish':False},ensure_ascii=False),'state':state,'generation':1 if state!='queued' else 0,'owner':'worker-1' if state!='queued' else None,'result':json.dumps({'outcome':outcome,'summary':'完成结果需要人工审阅'},ensure_ascii=False) if outcome else None,'continuation_status':None}
def review_task(i, outcome='failure'):
    value=task(i,'finished',outcome=outcome)
    job=json.loads(value['payload']);job['workflow']='reviewed'
    value['payload']=json.dumps(job,ensure_ascii=False)
    passed={'outcome':'success','exit_code':0,'summary':'测试通过'}
    value['result']=json.dumps({'outcome':outcome,'workspace':f'/fixture/task-{i}',
        'tests':dict(passed,signal=None,error=None),'draft_pr':None,'workflow':{'name':'reviewed','base_sha':'b'*40,
        'candidate_sha':'a'*40,'reviewed_sha':None,'publication':None,
        'reconciliation_required':False,'rounds':[{'round':0,'candidate_sha':'a'*40,
        'developer':passed,'tests':passed,'review':None,
        'reviewer':{'outcome':'failure','exit_code':1,'summary':'审查连接中断'}}]}},ensure_ascii=False)
    return value
def catalog_fixture():
    def evidence(state, reason, source='fixture:read-only-discovery'):
        return {'state':state,'reason':reason,'source':source}
    attack='<img src=x onerror=alert(1)>'
    return {'provider':'codex_app_server','checked_at_unix_ms':1700000000000,'cli_version':'fixture-cli 1.0',
        'executable':evidence('supported','Configured executable is runnable'),
        'compatibility':evidence('supported','Protocol verified'),
        'authentication':evidence('unknown','Account authentication is not checked'),
        'reviewer_isolation':evidence('unsupported','Read-only reviewer isolation is unavailable'),
        'permission_control':evidence('supported','Approval requests are rejected'),
        'session_continuity':evidence('supported','Explicit sessions are supported'),
        'startup_context':evidence('unknown','Discovery context differs from execution'),
        'process_cleanup':evidence('supported','Discovery process has stopped'),
        'model_catalog':evidence('supported','All catalog pages returned'),
        'models':[{'id':'fixture-model','model':'fixture-model','display_name':attack,
            'description':'Model metadata is plain text '+attack,'default_effort':'high',
            'supported_efforts':[{'effort':'low','description':'Fast'},{'effort':'high','description':attack}],
            'is_default':True,'hidden':False,'source':'fixture:model/list'}],
        'selection':{'requested_model':'manual-model','requested_effort':'high',
            'effective_model':None,'effective_effort':None,
            'status':evidence('unknown','Execution has not verified the requested configuration')}}
with sync_playwright() as p:
    browser=p.chromium.launch(executable_path=os.environ.get('CHROMIUM_PATH'),headless=True,args=['--no-sandbox'])
    context=browser.new_context(viewport={'width':1440,'height':1150},locale='zh-CN')
    page=context.new_page(); errors=[]; requests=[]; submissions=[]
    data={'tasks':[task(3,'queued'),task(2,'claimed'),task(1,'finished',outcome='success')], 'status':{'active':task(2,'claimed'),'recovery_required':False,'diagnostic':None},'post':'success','list_error':False,'continuations':{},'continuation_payloads':{},'continuation_post':'success','hidden_task_ids':set(),'frozen_lists':{},'retry_requests':[],'review_requests':[],'config':{'repositories':['relay-demo','api-service'],'agents':['codex','reviewer'],'tests':['unit','full']}}
    data['catalog']={'name':'codex','cache_epoch':'browser-fixture-process','generation':0,'stale':True,'refreshing':False,'catalog':None}
    data['catalog_refreshes']=0
    # Model persisted reservations separately from visible task payloads. A child
    # outside the recent page must not make its predecessor appear retryable.
    def task_response(value):
        return dict(value,continuation_status=data['continuations'].get(value['id']))
    def visible_tasks():
        return [task_response(value) for value in data['tasks'] if value['id'] not in data['hidden_task_ids']]
    def route(r):
        req=r.request; path=req.url.replace(base,''); requests.append((req.method,path,req.headers))
        assert req.headers.get('authorization')=='Bearer test-token',req.headers
        if path=='/api/config': result=data['config']
        elif path=='/api/status': result=data['status']
        elif path=='/api/capabilities':
            assert req.method=='GET'
            result={'profiles':[data['catalog']] if data['config'].get('native_agents') else []}
        elif path=='/api/capabilities/codex/refresh':
            assert req.method=='POST' and not req.post_data
            data['catalog_refreshes']+=1
            data['catalog'].update(generation=data['catalog']['generation']+1,stale=False,catalog=catalog_fixture())
            result=data['catalog']
        elif path=='/api/tasks' and req.method=='GET':
            if data['list_error']: r.abort();return
            result=data['frozen_lists'].get(req.frame.page,visible_tasks())
        elif path=='/api/tasks' and req.method=='POST':
            body=req.post_data_json; submissions.append(body)
            assert body['job']['publish'] is False
            if data['post']=='abort': r.abort();return
            if data['post']=='401': r.fulfill(status=401,content_type='application/json',body=json.dumps({'error':'Unauthorized'}));return
            result={'id':max(t['id'] for t in data['tasks'])+1,'key':body['key'],'payload':json.dumps(body['job'],ensure_ascii=False),'state':'queued','generation':0,'owner':None,'result':None,'continuation_status':None}
            data['tasks'].append(result)
        elif path.endswith(('/retry','/continue-review')):
            old_id=int(path.split('/')[-2]);body=req.post_data_json
            assert body['confirm_stopped_and_reconciled'] is True
            assert req.method=='POST'
            review=path.endswith('/continue-review')
            if review:
                assert set(body)=={'key','confirm_stopped_and_reconciled','revalidate_tests','review_focus'}
                assert body['revalidate_tests'] is True
                assert body['review_focus'] is None or 0<len(body['review_focus'].encode('utf-8'))<=8192
            else:
                assert set(body)=={'key','confirm_stopped_and_reconciled'}
            data['review_requests' if review else 'retry_requests'].append((old_id,body))
            if data['continuation_post']=='abort': r.abort();return
            old=next(t for t in data['tasks'] if t['id']==old_id)
            reservation=data['continuations'].setdefault(old_id,{'successor_id':None})
            # One persisted intent per predecessor, shared by both endpoints.
            # A later mode or key cannot rewrite the first accepted request.
            data['continuation_payloads'].setdefault(old_id,{'path':path,'body':body})
            if reservation['successor_id'] is None:
                job=json.loads(old['payload'])
                workspace_id=job.get('continuation',{}).get('workspace_task_id',old_id)
                job['continuation']={'workspace_task_id':workspace_id,'predecessor_task_id':old_id,'predecessor_generation':old['generation']}
                original=data['continuation_payloads'][old_id]
                if original['path'].endswith('/continue-review'):
                    workflow=json.loads(old['result'])['workflow']
                    job['continuation']['review_only']={'base_sha':workflow['base_sha'],'candidate_sha':workflow['candidate_sha'],'round':workflow['rounds'][-1]['round']}
                    if original['body']['review_focus'] is not None:
                        job['continuation']['review_only']['review_focus']=original['body']['review_focus']
                result=task(max(t['id'] for t in data['tasks'])+1);result['key']=original['body']['key'];result['payload']=json.dumps(job)
                data['tasks'].append(result)
                reservation['successor_id']=result['id']
            else:
                result=next(t for t in data['tasks'] if t['id']==reservation['successor_id'])
            # A stale tab receives the same authoritative successor, even when
            # it generated a fresh key and opened its dialog before another tab.
            data['frozen_lists'].pop(req.frame.page,None)
            result=task_response(result)
        elif path.endswith('/cancel'): result={'requested':True}
        elif path.startswith('/api/tasks/'): result=task_response(next(t for t in data['tasks'] if t['id']==int(path.rsplit('/',1)[-1])))
        else: raise Exception(path)
        r.fulfill(status=200,content_type='application/json',body=json.dumps(result,ensure_ascii=False))
    def instrument(target):
        target.on('pageerror',lambda e:errors.append(str(e)))
        target.route('**/auth/status', lambda r: r.fulfill(status=200,content_type='application/json',body=json.dumps({'mode':'bearer','authenticated':False})))
        target.route('**/api/**',route)
    def connect(target):
        target.locator('#token').fill('test-token');target.locator('#connect').click()
        expect(target.locator('#auth-panel')).to_be_hidden()
    def select_task(target,task_id):
        with target.expect_response(lambda response: response.url==base+'/api/tasks/'+str(task_id) and response.request.method=='GET'):
            target.locator('[data-task-id="'+str(task_id)+'"]').click()
        expect(target.locator('#detail-title')).to_have_text('任务 #'+str(task_id))
    def assert_successor(target,predecessor_id,successor_id):
        expect(target.locator('#detail-title')).to_have_text('任务 #'+str(predecessor_id))
        expect(target.locator('#retry-task')).to_be_hidden()
        expect(target.locator('#review-task')).to_be_hidden()
        expect(target.locator('#continuation-next')).to_have_text('已续接至任务 #'+str(successor_id))
        expect(target.locator('#continuation-next')).to_be_visible()
        expect(target.locator('#continuation-next')).to_be_enabled()
    instrument(page)
    page.goto(base); page.wait_for_timeout(2200)
    assert not requests, 'Requests before authentication'
    page.screenshot(path=str(SCREENSHOTS / 'relay-disconnected-desktop.png'),full_page=True)
    page.locator('#token').fill('test-token');page.locator('#connect').click();page.locator('#auth-panel').wait_for(state='hidden')
    assert page.locator('#token').input_value()==''
    assert page.evaluate('localStorage.length + sessionStorage.length')==0
    assert 'test-token' not in page.url
    assert page.locator('#queued-count').inner_text()=='1'
    assert page.locator('#claimed-count').inner_text()=='1'
    assert page.locator('#finished-count').inner_text()=='1'
    assert page.locator('.task-button').count()==3
    assert not page.locator('#workflow-field').is_visible(), 'Legacy configuration keeps the ordinary form'
    page.screenshot(path=str(SCREENSHOTS / 'relay-connected-desktop.png'),full_page=True)
    # Token details distinguish current-turn totals from the final snapshot.
    data['tasks'][2]['result']=json.dumps({'outcome':'success','agent':{'provider':{
        'provider':'codex_app_server','usage':{'usage_scope':'last_snapshot',
        'input_tokens':7,'output_tokens':2,'turn_total':{'input_tokens':90,
        'output_tokens':11,'cached_input_tokens':50,'reasoning_output_tokens':0}}}}})
    page.locator('[data-task-id="1"]').click()
    page.locator('#detail-usage').wait_for(state='visible')
    assert page.locator('.usage-heading').inner_text()=='本轮累计'
    assert page.locator('.usage-counts').first.locator('dd').all_inner_texts()==['90','11','50','0']
    assert page.locator('.usage-snapshot dd').all_inner_texts()==['7','2','未知','未知']
    page.screenshot(path=str(SCREENSHOTS / 'relay-token-desktop.png'),full_page=True)
    page.set_viewport_size({'width':390,'height':844})
    assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
    page.screenshot(path=str(SCREENSHOTS / 'relay-token-mobile.png'),full_page=True)
    page.set_viewport_size({'width':1440,'height':1150})
    page.locator('[data-task-id="3"]').click()
    assert not page.locator('#detail-usage').is_visible()
    # The catalog reads cached evidence on login/open, without automatic discovery.
    page.locator('#logout').click()
    data['config']['native_agents']=[{'name':'codex','provider':'codex_app_server','model':'manual-model','effort':'high','authentication':'unknown'}]
    with page.expect_response(lambda response: response.url==base+'/api/capabilities' and response.request.method=='GET'):
        connect(page)
    expect(page.locator('#capability-panel')).to_be_visible()
    expect(page.locator('#capability-body')).to_be_hidden()
    assert data['catalog_refreshes']==0
    with page.expect_response(lambda response: response.url==base+'/api/capabilities' and response.request.method=='GET'):
        page.locator('#capability-toggle').click()
    expect(page.locator('#capability-toggle')).to_have_attribute('aria-expanded','true')
    expect(page.locator('#capability-profiles')).to_contain_text('尚无缓存')
    expect(page.locator('#capability-disclaimer')).to_contain_text('不代表账户已登录、可调用模型或获得调用授权')
    page.wait_for_timeout(2200)
    assert data['catalog_refreshes']==0, 'Task polling must never trigger discovery'
    with page.expect_response(lambda response: response.url==base+'/api/capabilities/codex/refresh' and response.request.method=='POST'):
        page.locator('[data-catalog-refresh="codex"]').click()
    card=page.locator('[data-profile="codex"]')
    expect(card).to_contain_text('fixture-cli 1.0')
    expect(card).to_contain_text('上次读取时缓存有效')
    expect(card).to_contain_text('manual-model')
    expect(card).to_contain_text('未知（尚无执行证据）')
    expect(card).to_contain_text('Account authentication is not checked')
    expect(card).to_contain_text('Discovery context differs from execution')
    expect(card).to_contain_text('Read-only reviewer isolation is unavailable')
    card.locator('.catalog-models summary').click()
    expect(card.locator('.catalog-model-list')).to_be_visible()
    expect(card.locator('.catalog-model-list')).to_contain_text('<img src=x onerror=alert(1)>')
    expect(card.locator('.catalog-model-list')).to_contain_text('支持的 effort：low（Fast）、high')
    expect(card.locator('.catalog-model-list')).to_contain_text('来源：fixture:model/list')
    assert page.locator('img').count()==0, 'Catalog metadata must remain inert text'
    assert card.locator('select,input').count()==0, 'Phase-one catalog is read-only'
    assert data['catalog_refreshes']==1 and not submissions
    for width in [1440,390,320]:
        page.set_viewport_size({'width':width,'height':900})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'catalog overflow at {width}'
        page.screenshot(path=str(SCREENSHOTS / f'relay-catalog-{width}.png'),full_page=True)
    page.wait_for_timeout(2200)
    assert data['catalog_refreshes']==1, 'Open catalog must not repeat discovery'
    page.locator('#logout').click()
    expect(page.locator('#capability-panel')).to_be_hidden()
    expect(page.locator('#capability-body')).to_be_hidden()
    expect(page.locator('#capability-profiles')).to_be_empty()
    expect(page.locator('#capability-toggle')).to_have_attribute('aria-expanded','false')
    data['config'].pop('native_agents')
    page.set_viewport_size({'width':1440,'height':1150})
    # Optional named workflows lock configured fields, then restore ordinary choices.
    reviewed={'name':'reviewed','repository':'api-service','developer':'native-codex','reviewer':'reviewer <img src=x onerror=alert(1)>','test':'full','max_repairs':2}
    data['config']['agents'] += [reviewed['developer'],reviewed['reviewer']]
    data['config']['workflows']=[reviewed,dict(reviewed,name='review-only',max_repairs=0)]
    page.locator('#token').fill('test-token');page.locator('#connect').click();page.locator('#auth-panel').wait_for(state='hidden')
    assert page.locator('#workflow-field').is_visible()
    assert page.locator('#workflow').input_value()==''
    page.locator('#test').select_option('unit');page.locator('#workflow').select_option('reviewed')
    for field,value in [('repository','api-service'),('agent','native-codex'),('test','full')]:
        assert page.locator('#'+field).input_value()==value
        assert page.locator('#'+field).is_disabled()
    assert not page.locator('#requirements').is_disabled()
    assert '最多修复 2 轮' in page.locator('#workflow-hint').inner_text()
    assert '<img' in page.locator('#workflow-hint').inner_text()
    assert page.locator('img').count()==0
    page.locator('#workflow').select_option('review-only')
    assert '最多修复 0 轮' in page.locator('#workflow-hint').inner_text()
    page.locator('#workflow').select_option('')
    for field,value in [('repository','relay-demo'),('agent','codex'),('test','unit')]:
        assert page.locator('#'+field).input_value()==value
        assert not page.locator('#'+field).is_disabled()
    assert not page.locator('#workflow-hint').is_visible()
    page.locator('#workflow').select_option('reviewed')
    page.screenshot(path=str(SCREENSHOTS / 'relay-workflow-desktop.png'),full_page=True)
    # Secure text rendering and clear unknown status.
    data['tasks'][0]['payload']=json.dumps({'repository':'<img src=x onerror=alert(1)>','requirements':'<script>alert(1)</script>\n核查任务','agent':'codex','test':None,'publish':False})
    data['tasks'][0]['state']='finished';data['tasks'][0]['result']=json.dumps({'outcome':'unknown','summary':'<img src=x onerror=alert(1)>'})
    page.locator('#refresh').click();page.wait_for_timeout(150)
    assert page.locator('#detail-warning').is_visible()
    assert '<script>' in page.locator('#detail-requirements').inner_text()
    assert page.locator('img').count()==0
    # Filters and detail selection.
    page.locator('[data-filter=claimed]').click();assert page.locator('.task-button').count()==1
    page.locator('.task-button').click();assert page.locator('#detail-title').inner_text()=='任务 #2'
    page.locator('#cancel-task').click();assert page.locator('#cancel-dialog').is_visible()
    page.keyboard.press('Escape');assert not page.locator('#cancel-dialog').is_visible()
    page.locator('#cancel-task').click();page.locator('#cancel-confirm').click();page.wait_for_timeout(100)
    assert page.locator('#cancel-task').inner_text()=='已请求取消'
    assert page.locator('#cancel-task').is_disabled()
    # Ambiguous submission must preserve its exact key and payload; fields lock.
    page.locator('#requirements').fill('实现安全重试的任务流程')
    data['post']='abort';page.locator('#submit-task').click();page.wait_for_timeout(200)
    assert page.locator('#submit-task').inner_text()=='重试原提交'
    assert page.locator('#requirements').is_disabled()
    assert len(submissions)==1
    assert page.locator('#workflow').is_disabled()
    assert submissions[0]['job']=={'repository':'api-service','requirements':'实现安全重试的任务流程','agent':'native-codex','test':'full','publish':False,'workflow':'reviewed'}
    data['post']='success';page.locator('#submit-task').click();page.wait_for_timeout(200)
    assert len(submissions)==2 and submissions[0]==submissions[1]
    assert page.locator('#requirements').input_value()==''
    assert not page.locator('#requirements').is_disabled()
    assert page.locator('#detail-title').inner_text()=='任务 #4'
    assert 'reviewed' in page.locator('#detail-meta').inner_text()
    assert not page.locator('#workflow').is_disabled()
    assert page.locator('#repository').is_disabled()
    # Requirements are limited by UTF-8 bytes, independent of JSON payload cap.
    count = len(submissions)
    page.locator('#requirements').fill('界' * 11000)
    page.locator('#submit-task').click()
    assert len(submissions) == count
    assert '32 KiB' in page.locator('#form-message').inner_text()
    page.locator('#requirements').fill('')
    # Recovery disables further submission and exposes diagnostic as inert text.
    data['status']['recovery_required']=True;data['status']['diagnostic']=json.dumps({'outcome':'unknown','reason':'进程组状态待人工核对 <b>safe</b>'})
    page.locator('#refresh').click();page.wait_for_timeout(150)
    assert page.locator('#recovery-banner').is_visible()
    assert page.locator('#submit-task').is_disabled()
    page.locator('.task-button[data-task-id="2"]').click()
    assert page.locator('#detail-warning').is_visible()
    assert '<b>safe</b>' in page.locator('#detail-diagnostic-text').inner_text()
    page.locator('.task-button[data-task-id="4"]').click()
    assert not page.locator('#detail-diagnostic').is_visible()
    page.screenshot(path=str(SCREENSHOTS / 'relay-recovery-desktop.png'),full_page=True)
    # Network banner with last-known data.
    data['list_error']=True;page.locator('#refresh').click();page.wait_for_timeout(150)
    assert page.locator('#network-banner').is_visible()
    assert page.locator('.task-button').count()==4
    data['list_error']=False;page.locator('#refresh-error').click();page.wait_for_timeout(150)
    assert not page.locator('#network-banner').is_visible()
    # Mobile / dark / zoom-like narrow viewport: no horizontal scrolling.
    data['status']['recovery_required']=False;data['status']['diagnostic']=None;page.locator('#refresh').click();page.wait_for_timeout(150)
    for width in [390,320,768,1024,1440]:
        page.set_viewport_size({'width':width,'height':900})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'overflow at {width}'
    page.set_viewport_size({'width':390,'height':844});page.screenshot(path=str(SCREENSHOTS / 'relay-connected-mobile.png'),full_page=True)
    page.locator('#theme-toggle').click();page.screenshot(path=str(SCREENSHOTS / 'relay-connected-mobile-dark.png'),full_page=True)
    assert page.locator('html').get_attribute('data-theme')=='dark'
    # Auth interruption preserves the exact workflow request even when config changes.
    page.locator('#requirements').fill('保留已提交的审查工作流')
    data['post']='401';page.locator('#submit-task').click();page.locator('#auth-panel').wait_for(state='visible')
    original=submissions[-1]
    assert original['job']['workflow']=='reviewed'
    assert not page.locator('#workflow-field').is_visible()
    assert page.locator('#workflow').input_value()==''
    assert page.locator('#workflow-hint').inner_text()==''
    data['config']={'repositories':['relay-demo'],'agents':['codex'],'tests':['unit'],'workflows':[]}
    data['post']='success';page.locator('#token').fill('test-token');page.locator('#connect').click();page.locator('#auth-panel').wait_for(state='hidden')
    assert page.locator('#workflow').input_value()=='reviewed'
    assert page.locator('#workflow').is_disabled()
    assert page.locator('#agent').input_value()=='native-codex'
    page.locator('#submit-task').click();page.wait_for_timeout(200)
    assert submissions[-1]==original
    assert page.locator('#workflow').input_value()==''
    assert not page.locator('#workflow-field').is_visible()
    assert page.locator('#agent').input_value()=='codex'
    assert not page.locator('#agent').is_disabled()
    # Returning through browser history never restores a live authenticated form.
    page.goto(base+'/away');page.go_back();page.locator('#auth-panel').wait_for(state='visible')
    assert not page.locator('#workflow-field').is_visible()
    assert page.locator('#workflow').is_disabled()
    assert page.locator('#requirements').input_value()==''
    data['config']={'repositories':['relay-demo','api-service'],'agents':['codex',reviewed['developer'],reviewed['reviewer']],'tests':['unit','full'],'workflows':[reviewed]}
    page.locator('#token').fill('test-token');page.locator('#connect').click();page.locator('#auth-panel').wait_for(state='hidden')
    page.locator('#workflow').select_option('reviewed')
    # The persisted pageshow handler also clears a cached workflow selection.
    page.evaluate("window.dispatchEvent(new PageTransitionEvent('pageshow', {persisted: true}))")
    assert page.locator('#auth-panel').is_visible()
    assert page.locator('#workflow').input_value()==''
    assert not page.locator('#workflow-field').is_visible()
    assert page.locator('#workflow-hint').inner_text()==''
    page.locator('#token').fill('test-token');page.locator('#connect').click();page.locator('#auth-panel').wait_for(state='hidden')
    page.locator('#workflow').select_option('reviewed')
    # Explicit preserved-work continuation and confirmation fit desktop/mobile layouts.
    failed=task(120,'finished',outcome='failure');failed['result']=json.dumps({'outcome':'failure','workspace':'/fixture/task-120','draft_pr':None})
    data['tasks'].append(failed);data['status']={'active':None,'recovery_required':False,'diagnostic':None}
    page.locator('#refresh').click();page.wait_for_timeout(200);page.locator('[data-task-id="120"]').click();page.wait_for_timeout(100)
    assert page.locator('#retry-task').is_enabled()
    original_failed=json.loads(json.dumps(failed))
    # Keep a second tab on an old server snapshot, with its confirmation already
    # open. Its independent key must still return the first tab's successor.
    stale_page=context.new_page();instrument(stale_page);stale_page.goto(base);connect(stale_page)
    select_task(stale_page,120)
    data['frozen_lists'][stale_page]=json.loads(json.dumps(visible_tasks()))
    stale_page.locator('#retry-task').click()
    expect(stale_page.locator('#retry-dialog')).to_be_visible()
    page.locator('#retry-task').click();assert page.locator('#retry-dialog').is_visible()
    assert '不从源仓库重新复制' in page.locator('#retry-dialog').inner_text()
    page.screenshot(path=str(SCREENSHOTS / 'relay-continue-mobile.png'),full_page=True)
    page.locator('#retry-dismiss').click();assert not page.locator('#retry-dialog').is_visible()
    page.locator('#retry-task').click();page.locator('#retry-confirm').click();page.wait_for_timeout(200)
    assert page.locator('#detail-title').inner_text()=='任务 #121'
    assert '续接自' in page.locator('#detail-meta').inner_text()
    assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
    assert data['continuations'][120]=={'successor_id':121}
    assert len(data['retry_requests'])==1
    select_task(page,120);assert_successor(page,120,121)
    page.screenshot(path=str(SCREENSHOTS / 'relay-continued-original-mobile.png'),full_page=True)
    # Revisiting and repeatedly following an existing successor are GET-only.
    for _ in range(3):
        page.locator('#continuation-next').click()
        expect(page.locator('#detail-title')).to_have_text('任务 #121')
        select_task(page,120);assert_successor(page,120,121)
    assert len(data['retry_requests'])==1
    expect(stale_page.locator('#retry-dialog')).to_be_visible()
    stale_page.locator('#retry-confirm').click()
    expect(stale_page.locator('#detail-title')).to_have_text('任务 #121')
    assert len(data['retry_requests'])==2
    assert data['retry_requests'][0][1]['key']!=data['retry_requests'][1][1]['key']
    assert [value['id'] for value in data['tasks'] if value['id']>120]==[121]
    select_task(stale_page,120);assert_successor(stale_page,120,121)
    stale_page.close()
    # Both manual refresh and a full document reload use persisted state.
    with page.expect_response(lambda response: response.url==base+'/api/tasks'):
        page.locator('#refresh').click()
    assert_successor(page,120,121)
    page.reload();expect(page.locator('#auth-panel')).to_be_visible();connect(page)
    select_task(page,120);assert_successor(page,120,121)
    # A fresh browser context receives only the predecessor in its recent page.
    # The server status, not a visible child payload or browser storage, drives UI.
    data['hidden_task_ids'].add(121)
    fresh_context=browser.new_context(viewport={'width':1440,'height':1150},locale='zh-CN')
    fresh_page=fresh_context.new_page();instrument(fresh_page);fresh_page.goto(base);connect(fresh_page)
    assert fresh_page.evaluate('localStorage.length + sessionStorage.length')==0
    assert fresh_page.locator('[data-task-id="121"]').count()==0
    select_task(fresh_page,120);assert_successor(fresh_page,120,121)
    fresh_page.screenshot(path=str(SCREENSHOTS / 'relay-continued-original-desktop.png'),full_page=True)
    with fresh_page.expect_response(lambda response: response.url==base+'/api/tasks/121'):
        fresh_page.locator('#continuation-next').click()
    expect(fresh_page.locator('#detail-title')).to_have_text('任务 #121')
    with fresh_page.expect_response(lambda response: response.url==base+'/api/tasks/121'):
        fresh_page.locator('#refresh').click()
    expect(fresh_page.locator('#detail-title')).to_have_text('任务 #121')
    assert len(data['retry_requests'])==2
    # If that successor fails, only it can extend the chain. The original keeps
    # pointing at its first successor, not at the newest task in the workspace.
    child=next(value for value in data['tasks'] if value['id']==121)
    child.update(state='finished',generation=1,owner='worker-1',result=json.dumps({'outcome':'failure','workspace':'/fixture/task-120','draft_pr':None}))
    with fresh_page.expect_response(lambda response: response.url==base+'/api/tasks/121'):
        fresh_page.locator('#refresh').click()
    expect(fresh_page.locator('#retry-task')).to_be_visible()
    expect(fresh_page.locator('#retry-task')).to_be_enabled()
    expect(fresh_page.locator('#continuation-next')).to_be_hidden()
    fresh_page.locator('#retry-task').click();fresh_page.locator('#retry-confirm').click()
    expect(fresh_page.locator('#detail-title')).to_have_text('任务 #122')
    assert data['continuations'][121]=={'successor_id':122}
    grandchild=next(value for value in data['tasks'] if value['id']==122)
    assert json.loads(grandchild['payload'])['continuation']=={'workspace_task_id':120,'predecessor_task_id':121,'predecessor_generation':1}
    select_task(fresh_page,120);assert_successor(fresh_page,120,121)
    fresh_page.locator('#continuation-next').click()
    expect(fresh_page.locator('#detail-title')).to_have_text('任务 #121')
    assert_successor(fresh_page,121,122)
    assert len(data['retry_requests'])==3
    assert failed==original_failed, 'Continuing must not modify the original task or result'
    fresh_context.close();data['hidden_task_ids'].clear()
    # A durable reservation without a submitted child remains recoverable even
    # if the original result has no reusable-workspace field to rediscover.
    reserved=task(130,'finished',outcome='failure')
    data['tasks'].append(reserved);data['continuations'][130]={'successor_id':None}
    with page.expect_response(lambda response: response.url==base+'/api/tasks'):
        page.locator('#refresh').click()
    select_task(page,130)
    expect(page.locator('#retry-task')).to_have_text('恢复已预留的续接')
    expect(page.locator('#retry-task')).to_be_enabled()
    expect(page.locator('#continuation-next')).to_be_hidden()
    page.locator('#retry-task').click();page.locator('#retry-confirm').click()
    expect(page.locator('#detail-title')).to_have_text('任务 #131')
    assert data['continuations'][130]=={'successor_id':131}
    select_task(page,130);assert_successor(page,130,131)
    assert len(data['retry_requests'])==4
    # Review continuation is offered only for an interrupted reviewer on a
    # tested, unchanged candidate, with no recorded review/publication effects.
    interrupted=review_task(140);data['tasks'].append(interrupted)
    original_interrupted=json.loads(json.dumps(interrupted))
    with page.expect_response(lambda response: response.url==base+'/api/tasks'):
        page.locator('#refresh').click()
    select_task(page,140)
    expect(page.locator('#review-task')).to_have_text('仅继续审查（先复验测试）')
    expect(page.locator('#review-task')).to_be_enabled()
    pristine_result=json.loads(interrupted['result'])
    for outcome in ['failure','timed_out','cancelled']:
        interrupted['result']=json.dumps(dict(pristine_result,outcome=outcome))
        select_task(page,140);expect(page.locator('#review-task')).to_be_enabled()
    invalid_results=[]
    for field,value in [('outcome','unknown'),('workspace',None),('tests',{'outcome':'failure'}),('draft_pr',{'outcome':'failure'})]:
        invalid_results.append(dict(pristine_result,**{field:value}))
    for field,value in [('base_sha',None),('candidate_sha',None),('reviewed_sha','a'*40),('publication',{'dry_run':True}),('reconciliation_required',True),('rounds',[])]:
        invalid_results.append(dict(pristine_result,workflow=dict(pristine_result['workflow'],**{field:value})))
    for field,value in [('candidate_sha','c'*40),('tests',{'outcome':'failure'}),('reviewer',None),('review',{'verdict':'approved'})]:
        invalid_results.append(dict(pristine_result,workflow=dict(pristine_result['workflow'],rounds=[dict(pristine_result['workflow']['rounds'][0],**{field:value})])))
    for invalid in invalid_results:
        interrupted['result']=json.dumps(invalid);select_task(page,140)
        expect(page.locator('#review-task')).to_be_hidden()
    interrupted['result']=original_interrupted['result']
    without_workflow=json.loads(interrupted['payload']);without_workflow.pop('workflow')
    interrupted['payload']=json.dumps(without_workflow);select_task(page,140)
    expect(page.locator('#review-task')).to_be_hidden()
    interrupted['payload']=original_interrupted['payload'];select_task(page,140)
    # Dismissing either way has no side effect; the shared ordinary dialog must
    # not retain a discarded review focus or review-only explanatory text.
    before_reviews=len(data['review_requests']);before_retries=len(data['retry_requests'])
    page.locator('#review-task').click()
    expect(page.locator('#retry-dialog')).to_be_visible()
    expect(page.locator('#review-focus-field')).to_be_visible()
    expect(page.locator('#review-focus')).to_be_enabled()
    expect(page.locator('#retry-dialog-description')).to_contain_text('先重新运行配置的测试')
    expect(page.locator('#retry-dialog-description')).to_contain_text('不调用开发者')
    expect(page.locator('#retry-dialog-description')).to_contain_text('同一候选提交')
    expect(page.locator('#review-focus-hint')).to_contain_text('留空沿用原审查重点')
    page.locator('#review-focus').fill('此次重点不应因取消而提交')
    page.screenshot(path=str(SCREENSHOTS / 'relay-review-continue-mobile.png'),full_page=True)
    assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
    page.locator('#retry-dismiss').click();expect(page.locator('#retry-dialog')).to_be_hidden()
    page.locator('#retry-task').click()
    expect(page.locator('#review-focus-field')).to_be_hidden()
    expect(page.locator('#retry-dialog-description')).to_contain_text('重新执行开发')
    page.keyboard.press('Escape');expect(page.locator('#retry-dialog')).to_be_hidden()
    page.locator('#review-task').click();expect(page.locator('#review-focus')).to_have_value('')
    page.keyboard.press('Escape');expect(page.locator('#retry-dialog')).to_be_hidden()
    assert len(data['review_requests'])==before_reviews and len(data['retry_requests'])==before_retries
    # A stale second tab chooses ordinary continuation before the first tab
    # reserves review-only. Both modes still share one authoritative successor.
    stale_page=context.new_page();instrument(stale_page);stale_page.goto(base);connect(stale_page)
    select_task(stale_page,140)
    data['frozen_lists'][stale_page]=json.loads(json.dumps(visible_tasks()))
    stale_page.locator('#retry-task').click();expect(stale_page.locator('#retry-dialog')).to_be_visible()
    page.locator('#review-task').click()
    for invalid_focus in [' \n\t','界'*2731]:
        page.locator('#review-focus').fill(invalid_focus);page.locator('#retry-confirm').click()
        expect(page.locator('#retry-dialog')).to_be_visible()
        expect(page.locator('#retry-dialog-error')).to_contain_text('8192 UTF-8 字节')
        assert len(data['review_requests'])==before_reviews
    exact_focus='界'*2730+'ab'
    assert len(exact_focus.encode('utf-8'))==8192
    page.locator('#review-focus').fill(exact_focus)
    data['continuation_post']='abort';page.locator('#retry-confirm').click()
    expect(page.locator('#detail-error')).to_contain_text('续接未确认')
    assert len(data['review_requests'])==before_reviews+1
    original_review=data['review_requests'][-1]
    assert original_review[0]==140 and original_review[1]['review_focus']==exact_focus
    assert original_review[1]['revalidate_tests'] is True
    assert 140 not in data['continuations']
    expect(page.locator('#retry-task')).to_be_disabled()
    page.locator('#review-task').click()
    expect(page.locator('#review-focus')).to_have_value(exact_focus)
    expect(page.locator('#review-focus')).to_be_disabled()
    data['continuation_post']='success'
    # Synchronous synthetic clicks exercise the in-flight guard even if the
    # first handler closes the modal before the browser can deliver a second.
    page.evaluate("""() => {
        document.getElementById('review-focus').value = '不得替换未确认请求';
        const confirm = document.getElementById('retry-confirm');
        confirm.dispatchEvent(new MouseEvent('click', {bubbles:true}));
        confirm.dispatchEvent(new MouseEvent('click', {bubbles:true}));
    }""")
    expect(page.locator('#detail-title')).to_have_text('任务 #141')
    assert len(data['review_requests'])==before_reviews+2
    assert data['review_requests'][-1]==original_review
    expect(page.locator('#detail-meta')).to_contain_text('仅审查（先复验测试）')
    assert data['continuations'][140]=={'successor_id':141}
    accepted_review=json.loads(json.dumps(data['continuation_payloads'][140]))
    expect(stale_page.locator('#retry-dialog')).to_be_visible()
    stale_page.locator('#retry-confirm').click()
    expect(stale_page.locator('#detail-title')).to_have_text('任务 #141')
    assert len(data['retry_requests'])==before_retries+1
    assert data['retry_requests'][-1][1]['key']!=original_review[1]['key']
    assert data['continuation_payloads'][140]==accepted_review
    assert [value['id'] for value in data['tasks'] if value['id']>140]==[141]
    select_task(stale_page,140);assert_successor(stale_page,140,141);stale_page.close()
    select_task(page,140);assert_successor(page,140,141)
    for _ in range(2):
        page.locator('#continuation-next').click();expect(page.locator('#detail-title')).to_have_text('任务 #141')
        select_task(page,140);assert_successor(page,140,141)
    assert len(data['review_requests'])==before_reviews+2
    assert interrupted==original_interrupted, 'Review continuation must preserve the predecessor exactly'
    # Reload and off-page successors resolve from persistent status, with no
    # additional POST and without keeping credentials or state in web storage.
    data['hidden_task_ids'].add(141)
    page.reload();expect(page.locator('#auth-panel')).to_be_visible();connect(page)
    assert page.locator('[data-task-id="141"]').count()==0
    assert page.evaluate('localStorage.length + sessionStorage.length')==0
    select_task(page,140);assert_successor(page,140,141)
    page.locator('#continuation-next').click();expect(page.locator('#detail-title')).to_have_text('任务 #141')
    assert len(data['review_requests'])==before_reviews+2
    data['hidden_task_ids'].clear()
    # Reverse the race: a review-only confirmation opened before ordinary
    # continuation must return that original ordinary successor, not replace it.
    reverse=review_task(150);data['tasks'].append(reverse)
    with page.expect_response(lambda response: response.url==base+'/api/tasks'):
        page.locator('#refresh').click()
    select_task(page,150)
    stale_page=context.new_page();instrument(stale_page);stale_page.goto(base);connect(stale_page)
    select_task(stale_page,150)
    data['frozen_lists'][stale_page]=json.loads(json.dumps(visible_tasks()))
    stale_page.locator('#review-task').click();expect(stale_page.locator('#review-focus')).to_have_value('')
    page.locator('#retry-task').click();page.locator('#retry-confirm').click()
    expect(page.locator('#detail-title')).to_have_text('任务 #151')
    accepted_full=json.loads(json.dumps(data['continuation_payloads'][150]))
    assert accepted_full['path'].endswith('/retry')
    stale_page.locator('#retry-confirm').click()
    expect(stale_page.locator('#detail-title')).to_have_text('任务 #151')
    assert data['review_requests'][-1][0]==150
    assert data['review_requests'][-1][1]['review_focus'] is None
    assert data['continuation_payloads'][150]==accepted_full
    assert [value['id'] for value in data['tasks'] if value['id']>150]==[151]
    select_task(stale_page,150);assert_successor(stale_page,150,151);stale_page.close()
    select_task(page,150);assert_successor(page,150,151)
    # Logout clears sensitive task content and stops polling.
    page.locator('#logout').click();after_logout=len(requests);page.wait_for_timeout(2400)
    assert len(requests)==after_logout
    assert page.locator('#auth-panel').is_visible()
    assert not page.locator('#task-detail').is_visible()
    assert page.locator('#requirements').input_value()==''
    assert page.locator('#detail-requirements').inner_text()==''
    assert page.locator('#detail-result').inner_text()==''
    assert page.locator('#token').input_value()==''
    assert page.locator('#workflow').input_value()==''
    assert not page.locator('#workflow-field').is_visible()
    assert page.locator('#workflow-hint').inner_text()==''
    assert not errors, errors
    print('PASS: cached-only catalog login/open, explicit profile discovery, unknown auth/effective selection, startup context, safe model/effort metadata, catalog screenshots at 320/390/1440, no automatic discovery, catalog logout reset; optional workflows, configured-field locking/restoration, explicit workflow payload, workflow auth retry across config removal, browser-history and cached-page reset, auth, memory-only token, polling, secure rendering, filters, details, cancel modal, exact-key retry, recovery diagnostic, network recovery, widths 320/390/768/1024/1440, dark mode, persisted continuation status, repeated successor navigation, stale two-tab confirmation, reload/new-context recovery, off-page successor detail/refresh, continuation chains, reserved-submit recovery, review-only eligibility and dismissal, review focus UTF-8 boundary, immutable unknown-request retry, duplicate review confirmation, bidirectional cross-mode stale confirmations, review successor reload, logout; no browser errors')
    browser.close()
server.shutdown()
