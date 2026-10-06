"""Optional real-browser smoke test: python3 app/tests/ui_smoke.py.
Requires Python Playwright and an installed Chromium (or CHROMIUM_PATH).
Uses a local fixture, fake token, and no real repository execution.
The application itself has no JavaScript/build dependencies.
"""
import json, threading, tempfile, os, re, time
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
def resource_fixture():
    return {'host_policy_cap_bytes':104857600,'default_quota_bytes':104857600,'snapshot_cap_bytes':52428800,
        'initial_estimate':{'source':'host_inventory','snapshot_bytes':1000,'git_metadata_reference_bytes':500,
            'reviewer_copy_bytes':1000,'estimated_initial_bytes':2500,'complete':True,'notes':['初始清单估算，不保证后续构建完成']},
        'build_growth':'unknown','enforcement':'logical_bytes_best_effort','os_hard_quota':False,'disk_reserved':False}
def operator_fixture(value, reservation=None, saved=None):
    result=json.loads(value['result']) if value['result'] else {};job=json.loads(value['payload'])
    workflow=result.get('workflow') or {};rounds=workflow.get('rounds') or [];last=rounds[-1] if rounds else {}
    sha=workflow.get('candidate_sha');tests=result.get('tests') or {};last_tests=last.get('tests') or {}
    successor=(reservation or {}).get('successor_id')
    retry=value['state']=='finished' and result.get('outcome') in ['failure','timed_out','cancelled'] and successor is None and (reservation is not None or (result.get('workspace') and result.get('draft_pr') is None))
    review=retry and reservation is None and job.get('workflow') and job['workflow']==workflow.get('name') and all(isinstance(v,str) and re.fullmatch(r'(?:[0-9a-f]{40}|[0-9a-f]{64})',v) for v in [sha,workflow.get('base_sha')]) and workflow.get('reviewed_sha') is None and workflow.get('publication') is None and workflow.get('reconciliation_required') is False and last.get('candidate_sha')==sha and last.get('review') is None and isinstance(last.get('reviewer'),dict) and last_tests.get('outcome')=='success' and last_tests.get('exit_code')==0 and tests.get('outcome')=='success' and tests.get('exit_code')==0 and tests.get('signal') is None and tests.get('error') is None
    cap=104857600;quota=job.get('workspace_quota_bytes',cap)
    def action(name):
        return {'id':name,'quota_increase_allowed':quota<cap,'quota_increase_required':False,'min_quota_bytes':quota+1 if quota<cap else None,'max_quota_bytes':cap,'requires_test_revalidation':name=='continue_review'}
    frozen=None
    if saved:
        body=saved['body'];frozen={'action_id':'continue_review' if saved['path'].endswith('/continue-review') else 'retry','key':body['key'],'workspace_quota_bytes':body.get('workspace_quota_bytes'),'revalidate_tests':body.get('revalidate_tests',False),'review_focus':body.get('review_focus'),'replacement':body.get('replacement')}
    actions=[action('retry')] if retry else []
    if review: actions.append(action('continue_review'))
    if frozen and successor is None: actions=[action(frozen['action_id'])]
    return {'task_id':value['id'],'generation':value['generation'],'failure':result.get('failure'),
        'resources':{'usage':{'logical_bytes':4096,'complete':True,'measured_at':1700000000,'reason':None},'quota_bytes':quota,'host_policy_cap_bytes':cap,'snapshot_cap_bytes':52428800,'enforcement':'logical_bytes_best_effort','os_hard_quota':False,'disk_reserved':False},
        'retained_result':{'available':value['result'] is not None,'immutable':True},'workspace_retained':bool(result.get('workspace')),
        'recovery':{'inherited_quota_bytes':quota,'actions':actions,'blocked_reason':'发布已尝试，需先核对外部结果后本机恢复' if result.get('draft_pr') else None,'successor_id':successor,'reserved_request':frozen}}
def workspace_fixture(older=False):
    attack='<img src=x onerror=window.__inventoryXss=1>'
    def entry(i, status):
        return {'workspace_task_id':i,'path':'/fixture/task-'+str(i),
            'current_owner':{'task_id':i+100,'generation':2,'owner':'host-fixture','state':'finished'},
            'references':[{'task_id':i,'generation':1,'state':'finished','outcome':'failure'},
                {'task_id':i+100,'generation':2,'state':'finished','outcome':'success'}],
            'references_complete':True,'successor_reserved':False,
            'allocated_usage':{'allocated_bytes':4096,'complete':True,'measured_at':1700000000,'reason':None},
            'retention':{'status':status,'reason':'Host retention snapshot','eligible_at':1700003600}}
    entries=[entry(45,'protected')] if older else [entry(50-i,status) for i,status in enumerate(['eligible','waiting','disabled','protected','unknown'])]
    if not older:
        entries[0]['allocated_usage']['allocated_bytes']=0
        entries[1]['allocated_usage'].update(complete=False,reason='Bounded scan ended')
        entries[-1].update(path='/fixture/'+attack+'long-path-segment-'*18,current_owner=None,references_complete=False,successor_reserved=True)
        entries[-1]['allocated_usage'].update(allocated_bytes=None,complete=False,reason=attack)
        entries[-1]['references']=[{'task_id':1000,'generation':3,'state':attack,'outcome':None}]
        entries[-1]['retention'].update(reason=attack,eligible_at=None)
    return {'observed_at':1700000001,'policy':{'successful_retention_seconds':3600,'automatic_cleanup_enabled':True},
        'workspaces':entries,'next_before':None if older else 46,'complete':older,'reason':None if older else 'Some observations incomplete'}
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
def task_observation_fixture():
    attack='<img src=x onerror=window.__taskObservationXss=1>'
    return {'task_id':42,'repository':'relay-demo '+attack,'role':'reviewer',
        'cli_version':'2.1.291','checked_at_unix_ms':1700000010000,
        'requested_model':'observed-alias','requested_effort':'high','native_permission':'claude_restricted',
        'models':[{'id':'observed-alias','model':'observed-alias','display_name':'Observed task model',
            'description':attack,'default_effort':None,'supported_efforts':[{'effort':'high','description':None}],
            'is_default':None,'hidden':None,'source':'fixture:task_initialize',
            'resolved_model':'resolved-observed-alias','supports_effort':True,'supports_fast_mode':False}]}
with sync_playwright() as p:
    browser=p.chromium.launch(executable_path=os.environ.get('CHROMIUM_PATH'),headless=True,args=['--no-sandbox'])
    context=browser.new_context(viewport={'width':1440,'height':1150},locale='zh-CN')
    page=context.new_page(); errors=[]; requests=[]; submissions=[]
    data={'tasks':[task(3,'queued'),task(2,'claimed'),task(1,'finished',outcome='success')], 'status':{'active':task(2,'claimed'),'recovery_required':False,'diagnostic':None},'post':'success','list_error':False,'continuations':{},'continuation_payloads':{},'continuation_post':'success','hidden_task_ids':set(),'frozen_lists':{},'retry_requests':[],'review_requests':[],'config':{'repositories':['relay-demo','api-service'],'agents':['codex','reviewer'],'tests':['unit','full']}}
    data['catalog']={'name':'codex','cache_epoch':'browser-fixture-process','generation':0,'stale':True,'refreshing':False,'catalog':None}
    data['workspaces']=workspace_fixture();data['workspace_error']=False
    data['catalog_refreshes']=0;data['startup_refreshes']=0;data['startup_token_serial']=0
    data['resources']=resource_fixture();data['operator_overrides']={}
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
        elif path.startswith('/api/workspaces'):
            assert req.method=='GET' and not req.post_data
            if data['workspace_error']: r.fulfill(status=503,content_type='application/json',body=json.dumps({'error':'Workspace read unavailable'}));return
            assert path in ['/api/workspaces','/api/workspaces?before=46']
            result=workspace_fixture(True) if path.endswith('before=46') else data['workspaces']
        elif path.startswith('/api/resources?'): result=data['resources']
        elif path.endswith('/replacement-challenge'):
            assert req.method=='POST'
            body=req.post_data_json;old_id=int(path.split('/')[-2]);choice=body['replacement']
            assert 'confirm_permission_expansion' not in choice
            data.setdefault('replacement_challenges',[]).append((old_id,body))
            role='reviewer' if body['action']=='continue_review' else 'developer'
            profile=next(p for p in data['config']['native_agents'] if p['name']==choice['profile'])
            result={'challenge':f"{len(data['replacement_challenges'])+100:064x}",'expires_at_unix_ms':int(time.time()*1000)+data.get('replacement_expiry_ms',300000),
                'confirmation_text':'Confirm this stopped-stage replacement <img src=x>',
                'scope':{'predecessor_task_id':old_id,'action':body['action'],'role':role,'repository':'relay-demo','workflow':'reviewed',
                    role:{'profile':choice['profile'],'provider':profile['provider'],'model':choice.get('model',{}).get('value',profile['model']),
                        'effort':choice.get('effort',profile['effort']),'native_permission':choice.get('native_permission',profile['native_permission'])}}}
        elif path=='/api/permission-challenge':
            assert req.method=='POST'
            job=req.post_data_json['job'];data.setdefault('permission_challenges',[]).append(job)
            assert 'role_binding' not in job
            for choice in job.get('role_selections',{}).values(): assert 'confirm_permission_expansion' not in choice
            workflow=next((w for w in data['config'].get('workflows',[]) if w['name']==job.get('workflow')),None)
            def resolve_role(role):
                choice=job.get('role_selections',{}).get(role,{})
                name=choice.get('profile') or (job['agent'] if role=='developer' else workflow['reviewer'] if workflow else None)
                if name is None: return None
                profile=next(p for p in data['config']['native_agents'] if p['name']==name)
                return {'profile':name,'provider':profile['provider'],'model':choice.get('model',{}).get('value',profile['model']),
                    'effort':choice.get('effort',profile['effort']),'native_permission':choice.get('native_permission',profile['native_permission'])}
            result={'challenge':f"{len(data['permission_challenges']):064x}",'expires_at_unix_ms':int(time.time()*1000)+300000,
                'confirmation_text':'Confirm this exact host-resolved developer scope <img src=x>',
                'scope':{'repository':job['repository'],'workflow':job.get('workflow'),'developer':resolve_role('developer'),'reviewer':resolve_role('reviewer')}}
        elif path.endswith('/operator'):
            task_id=int(path.split('/')[-2]);value=next(t for t in data['tasks'] if t['id']==task_id)
            result=data['operator_overrides'].get(task_id) or operator_fixture(value,data['continuations'].get(task_id),data['continuation_payloads'].get(task_id))
        elif path=='/api/capabilities':
            assert req.method=='GET'
            profiles=data.get('catalogs', [data['catalog']])
            if any(profile['name']=='startup-claude' for profile in data['config'].get('native_agents',[])):
                scope=data['startup_catalog']['startup_discovery']
                if scope['confirmation_token'] is None or scope['expires_at_unix_ms']<=int(time.time()*1000):
                    data['startup_token_serial']+=1
                    scope.update(confirmation_token=f"browser-startup-{data['startup_token_serial']}",expires_at_unix_ms=int(time.time()*1000)+90000)
                profiles=[*profiles,data['startup_catalog']]
            result={'profiles':profiles if data['config'].get('native_agents') else []}
        elif path=='/api/capabilities/codex/refresh':
            assert req.method=='POST' and not req.post_data
            data['catalog_refreshes']+=1
            data['catalog'].update(generation=data['catalog']['generation']+1,stale=False,catalog=catalog_fixture())
            result=data['catalog']
        elif path=='/api/capabilities/startup-claude/refresh':
            scope=data['startup_catalog']['startup_discovery']
            assert req.method=='POST' and req.post_data_json=={'confirm_startup_effects':True,'confirmation_token':scope['confirmation_token']}
            assert scope['expires_at_unix_ms']>int(time.time()*1000)
            data['startup_refreshes']+=1
            scope.update(confirmation_token=None,expires_at_unix_ms=None)
            metadata=catalog_fixture();metadata.update(provider='claude_cli',cli_version='Claude startup fixture 2.1.291')
            metadata['models'][0]['source']='fixture:claude_initialize'
            data['startup_catalog'].update(generation=data['startup_catalog']['generation']+1,stale=False,catalog=metadata)
            result=data['startup_catalog']
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
                assert set(body)-{'workspace_quota_bytes','replacement','permission_challenge'}=={'key','confirm_stopped_and_reconciled','revalidate_tests','review_focus'}
                assert body['revalidate_tests'] is True
                assert body['review_focus'] is None or 0<len(body['review_focus'].encode('utf-8'))<=8192
            else:
                assert set(body)-{'workspace_quota_bytes','replacement','permission_challenge'}=={'key','confirm_stopped_and_reconciled'}
            data['review_requests' if review else 'retry_requests'].append((old_id,body))
            if data['continuation_post']=='abort': r.abort();return
            if data['continuation_post']=='401': r.fulfill(status=401,content_type='application/json',body=json.dumps({'error':'Unauthorized'}));return
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
                if 'workspace_quota_bytes' in original['body']: job['workspace_quota_bytes']=original['body']['workspace_quota_bytes']
                if original['path'].endswith('/continue-review'):
                    workflow=json.loads(old['result'])['workflow']
                    job['continuation']['review_only']={'base_sha':workflow['base_sha'],'candidate_sha':workflow['candidate_sha'],'round':workflow['rounds'][-1]['round']}
                    if original['body']['review_focus'] is not None:
                        job['continuation']['review_only']['review_focus']=original['body']['review_focus']
                if original['body'].get('replacement'):
                    role='reviewer' if original['path'].endswith('/continue-review') else 'developer'
                    choice=original['body']['replacement'];job.setdefault('role_selections',{})[role]=choice
                    if role=='developer': job['agent']=choice['profile']
                    job['continuation']['replacement']={'role':role,'session_epoch':1}
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
    # Workspace inventory is explicit GET-only, separate from task polling and resource quotas.
    workspace_reads=lambda: [path for method,path,_ in requests if path.startswith('/api/workspaces')]
    assert not workspace_reads()
    expect(page.locator('#inventory-panel')).to_be_visible()
    expect(page.locator('#inventory-body')).to_be_hidden()
    page.locator('#inventory-toggle').focus();page.keyboard.press('Enter')
    expect(page.locator('#inventory-toggle')).to_have_attribute('aria-expanded','true')
    assert not workspace_reads(), 'Opening a panel must not scan the host'
    page.locator('#inventory-read').focus()
    with page.expect_response(lambda response: response.url==base+'/api/workspaces'):
        page.keyboard.press('Space')
    expect(page.locator('#inventory-policy')).to_contain_text('3600 秒；自动清理已开启')
    expect(page.locator('#inventory-status')).to_contain_text('不完整')
    expect(page.locator('#inventory-disclaimer')).to_contain_text('不是可安全删除的结论或删除授权')
    expect(page.locator('#inventory-body')).to_contain_text('不是逻辑容量、独占占用或可回收空间')
    expect(page.locator('#inventory-body')).to_contain_text('旧配置根或其他路径未扫描')
    expect(page.locator('[data-workspace-task-id="50"]')).to_contain_text('0 B')
    expect(page.locator('[data-workspace-task-id="49"]')).to_contain_text('已观测 4.00 KiB')
    unknown=page.locator('[data-workspace-task-id="46"]')
    expect(unknown).to_contain_text('未知 / 未确认绑定')
    expect(unknown).to_contain_text('不完整，仍有未知引用')
    expect(unknown).to_contain_text('存在预留')
    expect(unknown).to_contain_text('<img src=x onerror=window.__inventoryXss=1>')
    assert page.locator('#inventory-entries img, #inventory-entries button, #inventory-entries a').count()==0
    assert page.evaluate('window.__inventoryXss') is None
    unknown.locator('summary').focus();page.keyboard.press('Enter')
    expect(unknown.locator('li')).to_contain_text('结果 未知')
    for width in [1440,390,320]:
        page.set_viewport_size({'width':width,'height':1000})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'workspace inventory overflow at {width}'
        assert page.locator('#inventory-panel').evaluate('(node) => node.scrollWidth <= node.clientWidth'),f'workspace panel overflow at {width}'
        page.locator('#inventory-panel').screenshot(path=str(SCREENSHOTS / f'relay-workspace-inventory-{width}.png'))
    page.set_viewport_size({'width':1440,'height':1150})
    inventory_count=len(workspace_reads());page.wait_for_timeout(2200);page.locator('#refresh').click()
    assert len(workspace_reads())==inventory_count, 'Task polling/refresh must not read workspaces'
    page.locator('#inventory-next').focus();page.keyboard.press('Enter')
    expect(page.locator('#inventory-page-status')).to_contain_text('第 2 页')
    expect(page.locator('[data-workspace-task-id="45"]')).to_be_visible()
    expect(page.locator('#inventory-next')).to_be_disabled()
    assert page.locator('[data-workspace-task-id="50"]').count()==0
    page.locator('#inventory-prev').focus();page.keyboard.press('Space')
    expect(page.locator('#inventory-page-status')).to_contain_text('第 1 页')
    expect(page.locator('#inventory-prev')).to_be_disabled()
    data['workspace_error']=True;page.locator('#inventory-read').click()
    expect(page.locator('#inventory-error')).to_contain_text('Workspace read unavailable')
    expect(page.locator('#inventory-status')).to_contain_text('保留资格未知')
    expect(page.locator('#inventory-next')).to_be_disabled()
    assert page.locator('.inventory-card').count()==0
    expect(page.locator('#inventory-policy')).to_be_empty()
    data['workspace_error']=False
    data['workspaces']['workspaces']=[];data['workspaces']['next_before']=None
    page.locator('#inventory-read').click();expect(page.locator('#inventory-entries')).to_contain_text('清单不完整')
    data['workspaces']['complete']=True
    data['workspaces']['policy']={'successful_retention_seconds':None,'automatic_cleanup_enabled':False}
    page.locator('#inventory-read').click();expect(page.locator('#inventory-entries')).to_contain_text('本页未发现工作区')
    expect(page.locator('#inventory-policy')).to_contain_text('未配置；自动清理已关闭')
    page.locator('#inventory-toggle').focus();page.keyboard.press('Space')
    expect(page.locator('#inventory-body')).to_be_hidden()
    expect(page.locator('#inventory-entries')).to_be_empty()
    data['workspaces']=workspace_fixture()
    # Estimates are bounded, explicit, selected-repository reads; task polling does not scan resources.
    expect(page.locator('#workspace-quota')).to_be_disabled()
    with page.expect_response(lambda response: '/api/resources?repository=relay-demo' in response.url):
        page.locator('#resource-read').click()
    expect(page.locator('#workspace-quota')).to_be_enabled()
    expect(page.locator('#resource-estimate')).to_contain_text('初始总量估算')
    expect(page.locator('#submission-resources')).to_contain_text('构建增长未知')
    expect(page.locator('#submission-resources')).to_contain_text('不预留主机磁盘')
    reads_before=len([path for method,path,_ in requests if path.startswith('/api/resources') or path.endswith('/operator')])
    page.wait_for_timeout(2200)
    assert len([path for method,path,_ in requests if path.startswith('/api/resources') or path.endswith('/operator')])==reads_before
    data['resources']['initial_estimate'].update(complete=False,estimated_initial_bytes=None,git_metadata_reference_bytes=None,notes=['<img src=x onerror=alert(1)>'])
    page.locator('#resource-read').click()
    expect(page.locator('#resource-estimate-status')).to_contain_text('估算不完整')
    expect(page.locator('#resource-estimate')).to_contain_text('初始总量估算：未知')
    assert page.locator('#resource-estimate img').count()==0
    for width in [1440,390,320]:
        page.set_viewport_size({'width':width,'height':900})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'resource composer overflow at {width}'
        page.screenshot(path=str(SCREENSHOTS / f'relay-resource-estimate-{width}.png'),full_page=True)
    page.set_viewport_size({'width':1440,'height':1150})
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
    data['config']['agents'].append('startup-claude')
    data['config']['native_agents']=[{'name':'codex','provider':'codex_app_server','model':'manual-model','effort':'high','authentication':'unknown','allow_startup_discovery':False},
        {'name':'startup-claude','provider':'claude_cli','model':None,'effort':None,'allow_startup_discovery':True}]
    data['startup_catalog']={'name':'startup-claude','cache_epoch':'browser-fixture-process','generation':0,'stale':True,'refreshing':False,'catalog':None,
        'startup_discovery':{'confirmation_token':None,'expires_at_unix_ms':None,
            'confirmation_text':'启动将沿用正常 managed/user hooks、policy/auth helpers、MCP/plugins，可能执行操作、访问网络并产生费用。Relay 仅发送 initialize，不发送模型任务提示；不保证没有副作用。<img src=x onerror=window.__startupXss=1>'}}
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
    # Opt-in Claude start is a separate explicit native-dialog flow. Merely opening
    # it reads the cache; cancel, Escape and focus-default Enter never start work.
    startup_button=page.locator('[data-catalog-refresh="startup-claude"]')
    assert data['startup_refreshes']==0
    with page.expect_response(lambda response: response.url==base+'/api/capabilities' and response.request.method=='GET'):
        startup_button.click()
    dialog=page.locator('#startup-discovery-dialog')
    expect(dialog).to_be_visible();expect(page.locator('#startup-discovery-title')).to_contain_text('startup-claude')
    expect(page.locator('#startup-discovery-description')).to_contain_text('managed/user hooks')
    expect(page.locator('#startup-discovery-description')).to_contain_text('访问网络并产生费用')
    assert page.locator('img').count()==0 and page.evaluate('window.__startupXss') is None
    assert data['startup_refreshes']==0
    for width in [1440,390,320]:
        page.set_viewport_size({'width':width,'height':650 if width==320 else 900})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'startup dialog overflow at {width}'
        bounds=dialog.bounding_box();assert bounds['x']>=0 and bounds['x']+bounds['width']<=width
        assert page.evaluate('document.getElementById("startup-discovery-dialog").scrollWidth <= document.getElementById("startup-discovery-dialog").clientWidth')
        page.screenshot(path=str(SCREENSHOTS / f'relay-startup-confirm-{width}.png'),full_page=True)
    page.locator('#startup-discovery-dismiss').click();expect(dialog).not_to_be_visible();assert data['startup_refreshes']==0
    startup_button.click();expect(dialog).to_be_visible();page.keyboard.press('Escape');expect(dialog).not_to_be_visible();assert data['startup_refreshes']==0
    startup_button.click();expect(dialog).to_be_visible();expect(page.locator('#startup-discovery-dismiss')).to_be_focused()
    page.keyboard.press('Enter');expect(dialog).not_to_be_visible();assert data['startup_refreshes']==0
    startup_button.click();expect(dialog).to_be_visible()
    with page.expect_response(lambda response: response.url==base+'/api/capabilities/startup-claude/refresh' and response.request.method=='POST'):
        page.locator('#startup-discovery-confirm').click()
    expect(dialog).not_to_be_visible();expect(page.locator('[data-profile="startup-claude"]')).to_contain_text('Claude startup fixture 2.1.291')
    assert data['startup_refreshes']==1 and not submissions
    page.wait_for_timeout(2200);assert data['startup_refreshes']==1, 'Confirmed discovery must not repeat itself'
    page.locator('#logout').click()
    expect(page.locator('#capability-panel')).to_be_hidden()
    expect(page.locator('#capability-body')).to_be_hidden()
    expect(page.locator('#capability-profiles')).to_be_empty()
    expect(page.locator('#capability-toggle')).to_have_attribute('aria-expanded','false')
    data['config'].pop('native_agents');data['config']['agents'].remove('startup-claude')
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
    page.locator('#refresh').click()
    # Refresh waits for both reads and may discard an overlapping stale revision.
    # Assert the rendered state, rather than assuming a 150 ms completion window.
    expect(page.locator('#recovery-banner')).to_be_visible()
    expect(page.locator('#submit-task')).to_be_disabled()
    select_task(page,2)
    expect(page.locator('#detail-warning')).to_be_visible()
    expect(page.locator('#detail-diagnostic-text')).to_contain_text('<b>safe</b>')
    select_task(page,4)
    expect(page.locator('#detail-diagnostic')).to_be_hidden()
    page.screenshot(path=str(SCREENSHOTS / 'relay-recovery-desktop.png'),full_page=True)
    # Network banner with last-known data.
    data['list_error']=True;page.locator('#refresh').click()
    expect(page.locator('#network-banner')).to_be_visible()
    expect(page.locator('.task-button')).to_have_count(4)
    data['list_error']=False;page.locator('#refresh-error').click()
    expect(page.locator('#network-banner')).to_be_hidden()
    # Mobile / dark / zoom-like narrow viewport: no horizontal scrolling.
    data['status']['recovery_required']=False;data['status']['diagnostic']=None;page.locator('#refresh').click()
    expect(page.locator('#recovery-banner')).to_be_hidden()
    expect(page.locator('#submit-task')).to_be_enabled()
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
    fresh_page.locator('#operator-read').click()
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
    # Resource recovery uses exact server eligibility, never failure-message heuristics.
    resource_task=task(180,'finished',outcome='failure')
    resource_job=json.loads(resource_task['payload']);resource_job['workspace_quota_bytes']=10485760;resource_task['payload']=json.dumps(resource_job)
    resource_task['result']=json.dumps({'outcome':'failure','workspace':'/fixture/task-180','draft_pr':None,'failure':{'code':'workspace_quota_exceeded','stage':'test','cause':'<img src=x onerror=alert(1)> logical capacity exceeded','required_bytes':20971520,'limit_bytes':10485760}})
    data['tasks'].append(resource_task)
    planned_operator=operator_fixture(resource_task)
    assert planned_operator['resources']['quota_bytes']==resource_job['workspace_quota_bytes']
    assert planned_operator['recovery']['inherited_quota_bytes']==resource_job['workspace_quota_bytes']
    planned_operator['recovery']['actions'][0].update(quota_increase_required=True,min_quota_bytes=20971520)
    data['operator_overrides'][180]=planned_operator
    with page.expect_response(lambda response: response.url==base+'/api/tasks'):
        page.locator('#refresh').click()
    select_task(page,180)
    expect(page.locator('#operator-failure')).to_contain_text('workspace_quota_exceeded')
    expect(page.locator('#operator-failure')).to_contain_text('<img src=x onerror=alert(1)>')
    assert page.locator('#operator-failure img').count()==0
    expect(page.locator('#operator-retained')).to_contain_text('已保留且不可改写')
    for label in ['已记录任务容量','无覆盖续接容量']:
        expect(page.locator('#operator-resources > div').filter(has_text=label)).to_contain_text('10.00 MiB（10485760 字节）')
    page.locator('#retry-task').click();page.locator('#retry-confirm').click()
    expect(page.locator('#retry-dialog')).to_be_visible()
    expect(page.locator('#retry-dialog-error')).to_contain_text('宿主要求显式提高容量')
    page.locator('#retry-quota').fill('20971520')
    expect(page.locator('#retry-dialog-error')).to_be_hidden()
    expect(page.locator('#retry-quota-summary')).to_contain_text('10485760 字节） → 20.00 MiB（20971520 字节）')
    for width in [1440,390,320]:
        page.set_viewport_size({'width':width,'height':900})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'resource recovery overflow at {width}'
        page.screenshot(path=str(SCREENSHOTS / f'relay-resource-recovery-{width}.png'),full_page=True)
    page.locator('#retry-dismiss').click()
    blocked=operator_fixture(resource_task);blocked['resources']['usage'].update(logical_bytes=None,complete=False,reason='Inventory incomplete');blocked['recovery'].update(actions=[],blocked_reason='Unknown process or publication requires host reconciliation')
    data['operator_overrides'][180]=blocked;page.locator('#operator-read').click()
    expect(page.locator('#retry-task')).to_be_disabled()
    expect(page.locator('#operator-resources')).to_contain_text('观测不完整')
    expect(page.locator('#operator-recovery')).to_contain_text('requires host reconciliation')
    blocked['resources']['quota_bytes']=blocked['resources']['host_policy_cap_bytes'];blocked['failure']['code']='source_snapshot_limit';page.locator('#operator-read').click()
    expect(page.locator('#operator-failure')).to_contain_text('source_snapshot_limit')
    expect(page.locator('#operator-recovery')).to_contain_text('经过授权的宿主配置变更')
    # Phase 3: independent roles, fresh catalog provenance and exact pending replay.
    data['frozen_lists'].pop(page,None)
    page.locator('#logout').click()
    def permission_mode(name, allowed=True):
        return {'id':name,'label':name,'host_allowed':allowed,'availability':'unknown' if allowed else 'unsupported',
            'reason':'Native availability is unknown <img src=x onerror=alert(1)>','reviewer_only':name=='claude_restricted',
            'requires_confirmation':name not in ['codex_workspace_write','claude_restricted'],'confirmation_text':None}
    def native_profile(name, provider='claude_cli'):
        return {'name':name,'provider':provider,'model':None,'effort':None,'native_permission':None,
            'reviewer_supported':provider=='claude_cli','permission_modes':
                [permission_mode('codex_workspace_write'),permission_mode('codex_full_access'),permission_mode('codex_auto_review')] if provider=='codex_app_server'
                else [permission_mode('claude_dont_ask'),permission_mode('claude_auto'),permission_mode('claude_bypass_permissions',False),permission_mode('claude_restricted')]}
    data['config']={'repositories':['relay-demo'],'agents':['codex','claude','other-review'],'tests':['unit'],
        'native_agents':[native_profile('codex','codex_app_server'),native_profile('claude'),native_profile('other-review')],
        'workflows':[{'name':'role-flow','repository':'relay-demo','developer':'codex','reviewer':'claude','test':'unit','max_repairs':1,
            'selectable_developers':['codex','claude'],'selectable_reviewers':['claude','other-review','codex']}]}
    role_catalog=catalog_fixture()
    role_catalog['models'].append(dict(role_catalog['models'][0],id='review-model',model='review-model',display_name='Independent reviewer model'))
    data['catalogs']=[{'name':name,'cache_epoch':'a'*32,'generation':1,'stale':False,'refreshing':False,'catalog':role_catalog} for name in ['codex','claude','other-review']]
    claude_catalog=next(item for item in data['catalogs'] if item['name']=='claude')
    claude_catalog.update(catalog=None,stale=True,task_observation=task_observation_fixture(),task_observation_stale=False)
    data['post']='success';page.locator('#token').fill('test-token');page.locator('#connect').click()
    expect(page.locator('#auth-panel')).to_be_hidden()
    page.locator('#workflow').select_option('role-flow');page.locator('#agent').select_option('claude')
    expect(page.locator('#reviewer option[value="codex"]')).to_be_disabled()
    # Task-scoped observations render without a standalone catalog and never become
    # verified model choices for either role. Reading them sends no model/task POST.
    observation_request_start=len(requests)
    page.locator('#capability-toggle').click()
    observed_card=page.locator('[data-profile="claude"]')
    observed=observed_card.locator('.catalog-task-observation')
    expect(observed).to_be_visible();expect(observed_card).to_contain_text('尚无缓存')
    for text in ['任务 #42','仓库：relay-demo','审查者（reviewer）','CLI 版本：2.1.291',
                 '上次读取时任务观察未过期','不是完整实时目录','不会用于已验证的目录选择',
                 '任务请求模型：observed-alias','原生模式：claude_restricted']:
        expect(observed).to_contain_text(text)
    observed.locator('summary').click()
    expect(observed.locator('.catalog-model-list')).to_be_visible()
    for text in ['初始化解析模型（非实际生效证明）：resolved-observed-alias','支持的 effort：high',
                 'Effort 支持：是','Fast mode 支持：否','Adaptive thinking 支持：未知（供应商未提供）',
                 'Auto mode 支持：未知（供应商未提供）','<img src=x onerror=window.__taskObservationXss=1>']:
        expect(observed).to_contain_text(text)
    assert observed.locator('img,script,select,input').count()==0
    assert page.evaluate('window.__taskObservationXss === undefined')
    for width in [1440,390,320]:
        page.set_viewport_size({'width':width,'height':900})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'task observation overflow at {width}'
        page.screenshot(path=str(SCREENSHOTS / f'relay-task-observation-{width}.png'),full_page=True)
    for role in ['developer','reviewer']:
        page.locator(f'#{role}-model-source').select_option('catalog')
        expect(page.locator(f'#{role}-model')).to_be_disabled()
        expect(page.locator(f'#{role}-model option')).to_have_count(1)
        expect(page.locator(f'#{role}-model option[value="observed-alias"]')).to_have_count(0)
        expect(page.locator(f'#{role}-effort')).to_be_disabled()
    claude_catalog['task_observation_stale']=True
    page.locator('#capability-read').click();expect(observed).to_contain_text('任务观察已陈旧')
    assert not any(method=='POST' for method,_,_ in requests[observation_request_start:])
    # Restore a separate fixture catalog for the existing role-selection scenarios.
    claude_catalog.update(catalog=role_catalog,stale=False,generation=2)
    page.locator('#capability-read').click()
    expect(page.locator('#developer-model option[value="fixture-model"]')).to_have_count(1)
    expect(page.locator('#reviewer-model option[value="observed-alias"]')).to_have_count(0)
    page.locator('#capability-toggle').click();expect(page.locator('#capability-body')).to_be_hidden()
    for role, model, effort in [('developer','fixture-model','high'),('reviewer','review-model','low')]:
        page.locator(f'#{role}-model-source').select_option('catalog')
        page.locator(f'#{role}-model').select_option(model);page.locator(f'#{role}-effort').select_option(effort)
    page.locator('#resource-read').click();expect(page.locator('#workspace-quota')).to_be_enabled()
    assert any('reviewer_profile=claude' in path for _,path,_ in requests)
    page.locator('#reviewer').select_option('other-review');expect(page.locator('#workspace-quota')).to_be_disabled()
    page.locator('#reviewer').select_option('claude')
    page.locator('#reviewer-model-source').select_option('catalog');page.locator('#reviewer-model').select_option('review-model');page.locator('#reviewer-effort').select_option('low')
    expect(page.locator('#developer-model')).to_have_value('fixture-model')
    expect(page.locator('#reviewer-support')).to_contain_text('不是原生 --permission-mode')
    assert page.locator('#developer-mode-reasons img').count()==0
    for width in [1440,390,320]:
        page.set_viewport_size({'width':width,'height':1000})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'role controls overflow at {width}'
        page.screenshot(path=str(SCREENSHOTS / f'relay-native-roles-{width}.png'),full_page=True)
    page.locator('#requirements').fill('Independent native role choices')
    page.locator('#submit-task').click();expect(page.locator('#form-message')).to_contain_text('已确认')
    role_job=submissions[-1]['job'];assert role_job['agent']=='claude'
    assert role_job['role_selections']['developer']['model']['value']=='fixture-model'
    assert role_job['role_selections']['reviewer']['model']['value']=='review-model'
    assert role_job['role_selections']['developer']['profile']==role_job['role_selections']['reviewer']['profile']=='claude'
    def confirm_permission():
        page.locator('#developer-challenge').click();expect(page.locator('#developer-confirm')).to_be_enabled()
        expect(page.locator('#developer-challenge-scope')).to_contain_text('profile=')
        expect(page.locator('#developer-confirm-text')).to_contain_text('Confirm this exact host-resolved')
        assert page.locator('#developer-confirm-text img').count()==0
        page.locator('#developer-confirm').check()
    # Default workflow returns to pinned profiles. Full access requires all three consequences.
    page.locator('#developer-permission').select_option('codex_full_access')
    expect(page.locator('#developer-confirm-text')).to_contain_text('expanded filesystem AND network access')
    expect(page.locator('#developer-confirm-text')).to_contain_text('no native approval prompts')
    page.locator('#requirements').fill('Risk confirmation fixture');count=len(submissions)
    page.locator('#submit-task').click();expect(page.locator('#form-message')).to_contain_text('风险确认');assert len(submissions)==count
    confirm_permission()
    # Codex native auto approval is separate from full access and has fresh consent.
    page.locator('#developer-permission').select_option('codex_auto_review')
    expect(page.locator('#developer-confirm')).not_to_be_checked()
    expect(page.locator('#developer-confirm')).to_be_disabled()
    expect(page.locator('#developer-permission-note')).to_contain_text('可能自动批准越界请求')
    expect(page.locator('#developer-confirm-text')).to_contain_text('审批模型由 Codex 选择')
    page.locator('#submit-task').click();assert len(submissions)==count
    confirm_permission()
    page.locator('#submit-task').click();expect(page.locator('#form-message')).to_contain_text('已确认')
    assert submissions[-1]['job']['role_selections']['developer']['native_permission']=='codex_auto_review'
    assert submissions[-1]['job']['role_selections']['developer']['confirm_permission_expansion'] is True
    # A profile change drops confirmation, mode and model. Auto is a classifier, not bypass.
    page.locator('#agent').select_option('claude');expect(page.locator('#developer-confirm')).not_to_be_checked()
    expect(page.locator('#developer-permission')).to_have_value('')
    page.locator('#developer-permission').select_option('claude_auto');expect(page.locator('#developer-permission-note')).to_contain_text('不是 bypass')
    expect(page.locator('#developer-permission-note')).to_contain_text('拒绝或回退')
    confirm_permission()
    for role, model, effort in [('developer','fixture-model','high'),('reviewer','review-model','low')]:
        page.locator(f'#{role}-model-source').select_option('catalog');page.locator(f'#{role}-model').select_option(model);page.locator(f'#{role}-effort').select_option(effort)
    confirm_permission()
    # Stale catalog explicitly invalidates both roles, no silently retained effort.
    for item in data['catalogs']: item['stale']=True
    page.locator('#capability-toggle').click();expect(page.locator('#developer-model')).to_have_value('')
    expect(page.locator('#developer-effort')).to_be_disabled()
    expect(page.locator('#developer-confirm')).not_to_be_checked();expect(page.locator('#developer-confirm')).to_be_disabled()
    page.locator('#developer-model-source').select_option('manual');page.locator('#developer-manual-model').fill('old-manual')
    expect(page.locator('#developer-model-note')).to_contain_text('未验证');expect(page.locator('#developer-effort')).to_be_disabled()
    for item in data['catalogs']: item.update(stale=False,generation=2)
    page.locator('#capability-read').click()
    page.locator('#reviewer-model').select_option('review-model');page.locator('#reviewer-effort').select_option('low')
    confirm_permission();page.locator('#developer-manual-model').fill('manual-unverified')
    expect(page.locator('#developer-confirm')).not_to_be_checked();expect(page.locator('#developer-confirm')).to_be_disabled()
    confirm_permission()
    # The preceding successful Auto-review submission cleared the form.
    page.locator('#requirements').fill('Preserve role retry after Auto-review submission')
    data['post']='401';page.locator('#submit-task').click();expect(page.locator('#auth-panel')).to_be_visible()
    original=json.loads(json.dumps(submissions[-1]));assert len(original['permission_challenge'])==64;assert original['job']['role_selections']['developer']['model']=={'value':'manual-unverified','source':'manual'}
    assert 'effort' not in original['job']['role_selections']['developer']
    data['config']={'repositories':[],'agents':[],'tests':[]};data['catalogs']=[]
    data['post']='success';page.locator('#token').fill('test-token');page.locator('#connect').click();expect(page.locator('#auth-panel')).to_be_hidden()
    for control in ['agent','reviewer','developer-manual-model','reviewer-model','developer-permission','developer-confirm']:
        expect(page.locator('#'+control)).to_be_disabled()
    expect(page.locator('#reviewer-model-note')).to_contain_text('原提交已冻结')
    page.locator('#submit-task').click();expect(page.locator('#form-message')).to_contain_text('已确认');assert submissions[-1]==original
    # Missing observed evidence stays unknown even with server-resolved session values.
    evidence={'requested':{'profile':'<img src=x>','provider':'claude_cli','model':'requested-model'},
        'session_settings':{'model':'session-only','source':'native session','approvals_reviewer':'auto_review'},'observed':{'model':None,'native_approval_reviews':[{'status':'denied','action_type':'networkAccess','rationale':'<img src=x> blocked','review_id':'native-review-1','source':'codex.item/autoApprovalReview/completed'}],'reroutes':[{'from_model':'requested-model','to_model':'rerouted-model','reason':'<script>not HTML</script>','thread_id':'thread','turn_id':'turn'}]},
        'verification':{'model':'unknown','effort':'unknown','permission':'unknown'},'truncated':True}
    role_task=data['tasks'][-1];role_task['state']='finished';role_task['result']=json.dumps({'agent':{'provider':{'selection':evidence}},'workflow':{'rounds':[{'round':0,'reviewer':{'outcome':'failure','exit_code':1,'summary':'Serialized StageSummary','selection':{'requested':{'model':'review-request'}}}}]}},ensure_ascii=False)
    page.locator('#refresh').click();expect(page.locator('#selection-stages')).to_contain_text('实际消息 / reroute 观测：模型 未知')
    expect(page.locator('#selection-stages')).to_contain_text('不是每回合证明')
    expect(page.locator('#selection-stages')).to_contain_text('review-request')
    expect(page.locator('#selection-stages')).to_contain_text('approvals_reviewer auto_review')
    expect(page.locator('#selection-stages')).to_contain_text('原生审批观测（不证明操作已执行）：denied')
    expect(page.locator('#selection-stages')).to_contain_text('证据已截断')
    assert page.locator('#selection-stages img, #selection-stages script').count()==0
    for width in [1440,390,320]:
        page.set_viewport_size({'width':width,'height':1000})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'role evidence overflow at {width}'
        page.screenshot(path=str(SCREENSHOTS / f'relay-native-role-evidence-{width}.png'),full_page=True)
    # Phase 4: optional same-stage replacement, exact consent and frozen first-winner replay.
    replacement_request_start=len(requests)
    page.locator('#logout').click()
    data['config']={'repositories':['relay-demo'],'agents':['codex','claude','other-review'],'tests':['unit'],
        'native_agents':[native_profile('codex','codex_app_server'),native_profile('claude'),native_profile('other-review')],
        'workflows':[{'name':'reviewed','repository':'relay-demo','developer':'codex','reviewer':'claude','test':'unit','max_repairs':2,
            'selectable_developers':['codex','claude'],'selectable_reviewers':['claude','other-review','codex']}]}
    data['catalogs']=[{'name':name,'cache_epoch':'d'*32,'generation':1,'stale':False,'refreshing':False,'catalog':role_catalog} for name in ['codex','claude','other-review']]
    def replacement_operator(value,role):
        result=operator_fixture(value)
        action=next(a for a in result['recovery']['actions'] if a['id']==('continue_review' if role=='reviewer' else 'retry'))
        action.update(ordinary_allowed=True,replacement={'allowed':True,'role':role,'reason':None,
            'profiles':['claude','other-review','codex'] if role=='reviewer' else ['codex','claude'],
            'stopped_stage':{'role':role,'round':1,'base_sha':'b'*40,'candidate_sha':'a'*40,'max_repairs':2,'remaining_repairs':1}})
        result['recovery']['actions']=[action]
        return result
    for task_id,role in [(220,'developer'),(221,'reviewer'),(222,'developer'),(223,'developer')]:
        value=review_task(task_id);data['tasks'].append(value);data['operator_overrides'][task_id]=replacement_operator(value,role)
    connect(page);select_task(page,220)
    page.locator('#retry-task').click();expect(page.locator('#retry-replace')).not_to_be_checked()
    page.locator('#retry-replace').check();page.locator('#replacement-profile').select_option('codex')
    page.locator('#replacement-model-source').select_option('catalog');page.locator('#replacement-model').select_option('fixture-model');page.locator('#replacement-effort').select_option('high')
    expect(page.locator('#replacement-stage')).to_contain_text('保留原轮次 1 与剩余修复预算 1')
    expect(page.locator('#retry-dialog-description')).to_contain_text('在已停止的开发阶段继续，保留原轮次和剩余修复预算')
    expect(page.locator('#replacement-session')).to_contain_text('旧原生历史不能跨供应商兼容恢复')
    page.locator('#replacement-permission').select_option('codex_full_access')
    expect(page.locator('#replacement-confirm-text')).to_contain_text('expanded filesystem AND network access')
    expect(page.locator('#replacement-confirm-text')).to_contain_text('no native approval prompts')
    count=len(data['retry_requests']);page.locator('#retry-confirm').click();expect(page.locator('#retry-dialog-error')).to_contain_text('精确权限范围');assert len(data['retry_requests'])==count
    def confirm_replacement():
        page.locator('#replacement-challenge').click();expect(page.locator('#replacement-confirm')).to_be_enabled()
        expect(page.locator('#replacement-challenge-scope')).to_contain_text('前置任务 #220')
        expect(page.locator('#replacement-confirm-text')).to_contain_text('<img src=x>')
        assert page.locator('#replacement-confirm-text img, #replacement-mode-reasons img').count()==0
        page.locator('#replacement-confirm').check()
        expect(page.locator('#retry-dialog-error')).to_be_hidden()
    data['replacement_expiry_ms']=-1;page.locator('#replacement-challenge').click()
    expect(page.locator('#replacement-challenge-status')).to_contain_text('读取失败');expect(page.locator('#replacement-confirm')).to_be_disabled()
    data['replacement_expiry_ms']=300000;confirm_replacement()
    page.locator('#replacement-model-source').select_option('manual');expect(page.locator('#replacement-confirm')).not_to_be_checked();expect(page.locator('#replacement-confirm')).to_be_disabled()
    page.locator('#replacement-manual-model').fill('replacement-unverified');expect(page.locator('#replacement-effort')).to_be_disabled();expect(page.locator('#replacement-model-note')).to_contain_text('未验证')
    confirm_replacement()
    for width in [1440,390,320]:
        page.set_viewport_size({'width':width,'height':1000})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'replacement dialog overflow at {width}'
        assert page.locator('#retry-dialog').evaluate('(node) => node.scrollWidth <= node.clientWidth'),f'replacement content overflow at {width}'
        page.locator('#retry-dialog').evaluate('(node) => { node.scrollTop = 0; }')
        page.screenshot(path=str(SCREENSHOTS / f'relay-stage-replacement-{width}.png'),full_page=True)
        page.locator('#replacement-confirm').scroll_into_view_if_needed()
        page.screenshot(path=str(SCREENSHOTS / f'relay-stage-replacement-consent-{width}.png'),full_page=True)
    data['continuation_post']='abort';page.locator('#retry-confirm').click();expect(page.locator('#detail-error')).to_contain_text('续接未确认')
    frozen=json.loads(json.dumps(data['retry_requests'][-1]));assert frozen[1]['replacement']['confirm_permission_expansion'] is True
    page.locator('#retry-task').click();expect(page.locator('#replacement-profile')).to_be_disabled();expect(page.locator('#replacement-manual-model')).to_have_value('replacement-unverified')
    data['continuation_post']='401';page.locator('#retry-confirm').click();expect(page.locator('#auth-panel')).to_be_visible();assert json.loads(json.dumps(data['retry_requests'][-1]))==frozen
    for item in data['catalogs']: item.update(cache_epoch='e'*32,generation=1,stale=True)
    connect(page);select_task(page,220);page.locator('#retry-task').click();expect(page.locator('#replacement-confirm')).to_be_disabled()
    data['continuation_post']='success';page.locator('#retry-confirm').click();expect(page.locator('#continuation-choice')).to_contain_text('replacement-unverified');assert json.loads(json.dumps(data['retry_requests'][-1]))==frozen
    # Reviewer replacement never asks for developer expansion consent and retains the exact candidate.
    select_task(page,221);page.locator('#review-task').click();page.locator('#retry-replace').check();page.locator('#replacement-profile').select_option('other-review')
    expect(page.locator('#replacement-profile option[value="codex"]')).to_be_disabled()
    expect(page.locator('#replacement-stage')).to_contain_text('a'*40)
    expect(page.locator('#replacement-stage')).to_contain_text('原配置测试只运行一次，通过后仅审查，不开发、不修复')
    page.locator('#replacement-permission').select_option('claude_restricted');expect(page.locator('#replacement-confirm-field')).to_be_hidden()
    for width in [1440,390,320]:
        page.set_viewport_size({'width':width,'height':1000});assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
        page.screenshot(path=str(SCREENSHOTS / f'relay-reviewer-replacement-{width}.png'),full_page=True)
    before_challenges=len(data['replacement_challenges']);page.locator('#retry-confirm').click();expect(page.locator('#continuation-choice')).to_contain_text('配置 other-review')
    assert len(data['replacement_challenges'])==before_challenges;assert data['review_requests'][-1][1]['revalidate_tests'] is True
    # A second tab may win with a different replacement; the frozen server choice is visible.
    select_task(page,222);page.locator('#retry-task').click();page.locator('#retry-replace').check();page.locator('#replacement-profile').select_option('codex')
    stale_page=context.new_page();instrument(stale_page);stale_page.goto(base);connect(stale_page);select_task(stale_page,222)
    data['frozen_lists'][page]=json.loads(json.dumps(visible_tasks()))
    stale_page.locator('#retry-task').click();stale_page.locator('#retry-replace').check();stale_page.locator('#replacement-profile').select_option('claude')
    stale_page.locator('#replacement-model-source').select_option('manual');stale_page.locator('#replacement-manual-model').fill('other-tab-winner');stale_page.locator('#retry-confirm').click()
    expect(stale_page.locator('#continuation-choice')).to_contain_text('other-tab-winner')
    page.locator('#retry-confirm').click();expect(page.locator('#continuation-choice')).to_contain_text('other-tab-winner');expect(page.locator('#continuation-choice')).to_contain_text('其他页面的预留已获确认')
    assert data['continuation_payloads'][222]['body']['replacement']['profile']=='claude';stale_page.close()
    # Missing legacy stage proof and explicit publication ambiguity remain unavailable.
    select_task(page,223)
    unavailable=data['operator_overrides'][223]['recovery']['actions'][0]['replacement'];unavailable.update(allowed=False,reason='Missing stopped-stage proof; publication or unknown execution requires reconciliation',stopped_stage=None)
    page.locator('#operator-read').click();expect(page.locator('#operator-replacement')).to_contain_text('Missing stopped-stage proof')
    page.locator('#retry-task').click();expect(page.locator('#retry-replace')).to_be_disabled();expect(page.locator('#retry-replacement-status')).to_contain_text('requires reconciliation');page.locator('#retry-dismiss').click()
    assert not any(path.endswith('/refresh') for _,path,_ in requests[replacement_request_start:]), 'Replacement must not start catalog discovery'
    page.locator('#logout').click();after_logout=len(requests);page.wait_for_timeout(2400)
    assert len(requests)==after_logout
    assert page.locator('#auth-panel').is_visible()
    assert not page.locator('#task-detail').is_visible()
    assert page.locator('#requirements').input_value()==''
    assert page.locator('#detail-requirements').inner_text()==''
    assert page.locator('#detail-result').inner_text()==''
    assert page.locator('#token').input_value()==''
    expect(page.locator('#inventory-panel')).to_be_hidden()
    expect(page.locator('#inventory-entries')).to_be_empty()
    expect(page.locator('#inventory-policy')).to_be_empty()
    assert page.locator('#workflow').input_value()==''
    assert not page.locator('#workflow-field').is_visible()
    assert page.locator('#workflow-hint').inner_text()==''
    assert not errors, errors
    print('PASS: opt-in Claude fresh-GET/native-dialog/Cancel/Escape/default-focus/confirmed-start with inert text and 320/390/1440 screenshots; explicit workspace inventory reads/pagination/keyboard controls, allocated-vs-logical/reclaimable semantics, unknown/incomplete/error/empty states, inert XSS, no polling, responsive 320/390/1440; cached-only catalog login/open, explicit profile discovery, unknown auth/effective selection, startup context, safe model/effort metadata, task-only observations with scoped context/resolved alias/false-vs-unknown/staleness/inert XSS and no selector promotion, catalog screenshots at 320/390/1440, no automatic discovery, catalog logout reset; optional workflows, configured-field locking/restoration, explicit workflow payload, workflow auth retry across config removal, browser-history and cached-page reset, auth, memory-only token, polling, secure rendering, filters, details, cancel modal, exact-key retry, recovery diagnostic, network recovery, widths 320/390/768/1024/1440, dark mode, persisted continuation status, repeated successor navigation, stale two-tab confirmation, reload/new-context recovery, off-page successor detail/refresh, continuation chains, reserved-submit recovery, review-only eligibility and dismissal, review focus UTF-8 boundary, immutable unknown-request retry, duplicate review confirmation, bidirectional cross-mode stale confirmations, review successor reload, logout; no browser errors')
    browser.close()
server.shutdown()
