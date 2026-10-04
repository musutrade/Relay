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
    data={'tasks':[task(3,'queued'),task(2,'claimed'),task(1,'finished',outcome='success')], 'status':{'active':task(2,'claimed'),'recovery_required':False,'diagnostic':None},'post':'success','list_error':False}
    def route(r):
        req=r.request; path=req.url.replace(base,''); requests.append((req.method,path,req.headers))
        assert req.headers.get('authorization')=='Bearer test-token',req.headers
        if path=='/api/config': result={'repositories':['relay-demo','api-service'],'agents':['codex','reviewer'],'tests':['unit','full']}
        elif path=='/api/status': result=data['status']
        elif path=='/api/tasks' and req.method=='GET':
            if data['list_error']: r.abort();return
            result=data['tasks']
        elif path=='/api/tasks' and req.method=='POST':
            body=req.post_data_json; submissions.append(body)
            assert body['job']['publish'] is False
            if data['post']=='abort': r.abort();return
            result={'id':max(t['id'] for t in data['tasks'])+1,'key':body['key'],'payload':json.dumps(body['job'],ensure_ascii=False),'state':'queued','generation':0,'owner':None,'result':None}
            data['tasks'].append(result)
        elif path.endswith('/cancel'): result={'requested':True}
        elif path.startswith('/api/tasks/'): result=next(t for t in data['tasks'] if t['id']==int(path.rsplit('/',1)[-1]))
        else: raise Exception(path)
        r.fulfill(status=200,content_type='application/json',body=json.dumps(result,ensure_ascii=False))
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
    page.screenshot(path=str(SCREENSHOTS / 'relay-connected-desktop.png'),full_page=True)
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
    data['post']='success';page.locator('#submit-task').click();page.wait_for_timeout(200)
    assert len(submissions)==2 and submissions[0]==submissions[1]
    assert page.locator('#requirements').input_value()==''
    assert not page.locator('#requirements').is_disabled()
    assert page.locator('#detail-title').inner_text()=='任务 #4'
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
    # Logout clears sensitive task content and stops polling.
    page.locator('#logout').click();after_logout=len(requests);page.wait_for_timeout(2400)
    assert len(requests)==after_logout
    assert page.locator('#auth-panel').is_visible()
    assert not page.locator('#task-detail').is_visible()
    assert page.locator('#requirements').input_value()==''
    assert page.locator('#detail-requirements').inner_text()==''
    assert page.locator('#detail-result').inner_text()==''
    assert page.locator('#token').input_value()==''
    assert not errors, errors
    print('PASS: auth, memory-only token, polling, secure rendering, filters, details, cancel modal, exact-key retry, recovery diagnostic, network recovery, widths 320/390/768/1024/1440, dark mode, logout; no browser errors')
    browser.close()
server.shutdown()
