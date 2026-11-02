"""Synthetic shared-worker integration through the real CLI. No model calls."""
import argparse, concurrent.futures, json, os, shutil, subprocess, time, uuid
from pathlib import Path

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--cli',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args(); cli=args.cli.resolve()
    root=args.output.resolve()/('plugin worker '+str(uuid.uuid4())); root.mkdir(parents=True)
    dll=root/'fixture.dll'
    subprocess.run(['rustc','--crate-type','cdylib','--edition','2021',str(Path(__file__).parent/'fixtures/plugin-worker.rs'),'-o',str(dll)],check=True)
    def call(method,params=None,ok=True):
        cmd=[str(cli),'--root',str(root),'--no-background-jobs','call',method,'--input','-']
        result=subprocess.run(cmd,input=json.dumps(params or {}).encode(),stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=115)
        value=json.loads(result.stdout.decode('utf-8-sig'))
        assert value['ok']==ok,(method,value,result.stderr)
        return value.get('data',value)
    def backend():
        result=subprocess.run([str(cli),'--root',str(root),'backend','status'],capture_output=True,timeout=15)
        return json.loads(result.stdout.decode('utf-8-sig'))['data']
    def install(name,settings=True):
        directory=root/'packages'/name;directory.mkdir(parents=True)
        shutil.copy2(dll,directory/'fixture.dll');(directory/'settings.html').write_text('<p>Synthetic</p>')
        manifest={'id':name,'name':'Synthetic','version':'1','api_version':1,'entrypoint':'fixture.dll','files':['fixture.dll','settings.html'],
            'capabilities':['discover','validate','fetch_incremental','fetch_range','lookup_messages']}
        if settings: manifest['settings']={'api_version':1,'entrypoint':'settings.html','methods':['inspect','error','panic','abort','timeout','invalid','oversized']}
        path=directory/'plugin.json';path.write_text(json.dumps(manifest))
        call('plugin_install',{'path':str(path)})
        return path
    def session(plugin='synthetic-a'):
        return call('source_discover',{'plugin_id':plugin,'config':{}})['settings']['session']
    def invoke(s,method='inspect',ok=True): return call('plugin_settings',{'session':s,'action':'call','method':method,'params':{}},ok)
    checks=[]
    try:
        path=install('synthetic-a');install('synthetic-b');install('synthetic-reader',False)
        first=session();second=session('synthetic-b');a=invoke(first);b=invoke(second)
        assert a['pid']==b['pid'] and a['calls']==b['calls']==1
        pid=backend()['pid'];assert pid!=a['pid'];checks.append('single shared process, separate cached DLLs')
        for index in range(1200):
            last=invoke(first)
            if (index+1)%200==0:print(f'Checked {index+1} repeated calls',flush=True)
        assert last['pid']==a['pid'] and last['calls']==1201 and last['tls']==1201
        read=call('source_discover',{'plugin_id':'synthetic-reader','config':{}});assert read['pid']==a['pid']
        checks.append('1200 repeated ABI calls and discovery reuse')
        for method in ['error','panic','invalid','oversized']:
            invoke(first,method,False);assert invoke(first)['pid']==a['pid']
        checks.append('business errors, panic, malformed and oversize responses preserve worker')
        invoke(first,'abort',False);assert backend()['pid']==pid;recovered=invoke(first);assert recovered['pid']!=a['pid']
        checks.append('native abort isolated and next call recovers')
        with concurrent.futures.ThreadPoolExecutor() as pool:
            pending=pool.submit(invoke,first,'timeout',False);time.sleep(1)
            call('plugin_settings',{'session':first,'action':'cancel'});pending.result()
        assert backend()['pid']==pid;first=session();assert invoke(first)['pid']!=recovered['pid']
        checks.append('cancellation stops worker and leaves host alive')
        invoke(first,'timeout',False);assert backend()['pid']==pid;assert invoke(first)['pid']
        checks.append('90 second timeout, resource recovery')
        invoke(first,'abort',False);invoke(first,'abort',False)
        blocked=invoke(first,ok=False);assert '退避' in blocked['error']
        time.sleep(4.1);assert invoke(first)['pid'];checks.append('consecutive crashes back off without restart loop')
        source=call('source_save',{'source':{'plugin_id':'synthetic-reader','name':'Synthetic reader','config':{},'enabled':True}})
        call('scan_only');saved=next(s for s in call('plugins')['sources'] if s['id']==source['id'])
        cursor=saved['cursor'];saved['config']={'mode':'crash_scan'}
        source=call('source_save',{'source':saved})
        # Config changes intentionally reset progress; the failed scan must
        # preserve that committed cursor, not publish a partial response.
        before=next(s for s in call('plugins')['sources'] if s['id']==source['id'])['cursor']
        call('scan_only',ok=False);assert backend()['pid']==pid
        assert next(s for s in call('plugins')['sources'] if s['id']==source['id'])['cursor']==before
        assert invoke(first)['pid'];checks.append('scan crash leaves host and cursor intact')
        old=invoke(first)['pid'];call('plugin_install',{'path':str(path)});assert invoke(session())['pid']!=old
        checks.append('package update releases DLLs and recreates worker')
        (root/'result.json').write_text(json.dumps({'checks':checks},indent=2))
        print(json.dumps({'passed':len(checks),'checks':checks,'output':str(root)},ensure_ascii=False))
    finally:
        subprocess.run([str(cli),'--root',str(root),'backend','stop','--force'],capture_output=True,timeout=40)

if __name__=='__main__': main()
