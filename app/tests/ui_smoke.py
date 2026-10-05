"""Optional real-browser smoke test: python3 app/tests/ui_smoke.py.
Requires Python Playwright and an installed Chromium (or CHROMIUM_PATH).
Uses a local fixture, fake token, and no real repository execution.
The application itself has no JavaScript/build dependencies.
"""
import json, threading, tempfile, os
from pathlib import Path
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from playwright.sync_api import sync_playwright
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
    return {'id':i,'key':f'key-{i}','payload':json.dumps({'repository':'relay-demo','requirements':requirement or f'改进工作台的任务体验 #{i}','agent':'codex','test':'unit','publish':False},ensure_ascii=False),'state':state,'generation':1 if state!='queued' else 0,'owner':'worker-1' if state!='queued' else None,'result':json.dumps({'outcome':outcome,'summary':'完成结果需要人工审阅'},ensure_ascii=False) if outcome else None}
with sync_playwright() as p:
    browser=p.chromium.launch(executable_path=os.environ.get('CHROMIUM_PATH'),headless=True,args=['--no-sandbox'])
    context=browser.new_context(viewport={'width':1440,'height':1150},locale='zh-CN')
    page=context.new_page(); errors=[]; requests=[]; submissions=[]
    page.on('pageerror',lambda e:errors.append(str(e)))
    data={'tasks':[task(3,'queued'),task(2,'claimed'),task(1,'finished',outcome='success')], 'status':{'active':task(2,'claimed'),'recovery_required':False,'diagnostic':None},'post':'success','list_error':False,'config':{'repositories':['relay-demo','api-service'],'agents':['codex','reviewer'],'tests':['unit','full']}}
    def route(r):
        req=r.request; path=req.url.replace(base,''); requests.append((req.method,path,req.headers))
        assert req.headers.get('authorization')=='Bearer test-token',req.headers
        if path=='/api/config': result=data['config']
        elif path=='/api/status': result=data['status']
        elif path=='/api/tasks' and req.method=='GET':
            if data['list_error']: r.abort();return
            result=data['tasks']
        elif path=='/api/tasks' and req.method=='POST':
            body=req.post_data_json; submissions.append(body)
            assert body['job']['publish'] is False
            if data['post']=='abort': r.abort();return
            if data['post']=='401': r.fulfill(status=401,content_type='application/json',body=json.dumps({'error':'Unauthorized'}));return
            result={'id':max(t['id'] for t in data['tasks'])+1,'key':body['key'],'payload':json.dumps(body['job'],ensure_ascii=False),'state':'queued','generation':0,'owner':None,'result':None}
            data['tasks'].append(result)
        elif path.endswith('/retry'):
            old_id=int(path.split('/')[-2]);body=req.post_data_json
            assert body['confirm_stopped_and_reconciled'] is True
            old=next(t for t in data['tasks'] if t['id']==old_id)
            result=next((t for t in data['tasks'] if json.loads(t['payload']).get('continuation',{}).get('predecessor_task_id')==old_id),None)
            if result is None:
                job=json.loads(old['payload']);job['continuation']={'workspace_task_id':old_id,'predecessor_task_id':old_id,'predecessor_generation':1}
                result=task(max(t['id'] for t in data['tasks'])+1);result['key']=body['key'];result['payload']=json.dumps(job)
                data['tasks'].append(result)
        elif path.endswith('/cancel'): result={'requested':True}
        elif path.startswith('/api/tasks/'): result=next(t for t in data['tasks'] if t['id']==int(path.rsplit('/',1)[-1]))
        else: raise Exception(path)
        r.fulfill(status=200,content_type='application/json',body=json.dumps(result,ensure_ascii=False))
    page.route('**/auth/status', lambda r: r.fulfill(status=200,content_type='application/json',body=json.dumps({'mode':'bearer','authenticated':False})))
    page.route('**/api/**',route)
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
    # Optional named workflows lock configured fields, then restore ordinary choices.
    page.locator('#logout').click()
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
    page.locator('#retry-task').click();assert page.locator('#retry-dialog').is_visible()
    assert '不从源仓库重新复制' in page.locator('#retry-dialog').inner_text()
    page.screenshot(path=str(SCREENSHOTS / 'relay-continue-mobile.png'),full_page=True)
    page.locator('#retry-dismiss').click();assert not page.locator('#retry-dialog').is_visible()
    page.locator('#retry-task').click();page.locator('#retry-confirm').click();page.wait_for_timeout(200)
    assert page.locator('#detail-title').inner_text()=='任务 #121'
    assert '续接自' in page.locator('#detail-meta').inner_text()
    assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
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
    print('PASS: optional workflows, configured-field locking/restoration, explicit workflow payload, workflow auth retry across config removal, browser-history and cached-page reset, auth, memory-only token, polling, secure rendering, filters, details, cancel modal, exact-key retry, recovery diagnostic, network recovery, widths 320/390/768/1024/1440, dark mode, logout; no browser errors')
    browser.close()
server.shutdown()
