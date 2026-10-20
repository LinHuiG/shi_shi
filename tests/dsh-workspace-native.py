"""No-inference check against a real installed DSH in a new synthetic home."""
import argparse,json,os,subprocess,threading,queue,time
from pathlib import Path

def main():
 parser=argparse.ArgumentParser();parser.add_argument('--launcher',required=True);parser.add_argument('--output',required=True);args=parser.parse_args()
 root=Path(args.output).absolute()
 if root.exists():raise RuntimeError('A new isolated output directory is required')
 root.mkdir(parents=True);work=root/'workspace';work.mkdir();links=root/'links';links.mkdir()
 repo=Path(__file__).resolve().parents[1]
 patch=root/'fixture.patch.yml'
 rows=[{'id':'session-persistence-jsonl','config':{'root':str(root/'home/sessions')}},
       {'id':'storage-json','config':{'root':str(root/'home/storages')}},
       {'insert':[{'id':'ss-native-workspace','name':'@deepseek-ai/dsh-workspace'},
       {'id':'ss-native-bridge','name':str(repo/'examples/dsh-workspace-bridge/index.js'),'config':{'indexDir':str(links)}}]}]
 # JSON is a YAML subset; no credentials, personal profile or prompt is copied.
 patch.write_text(json.dumps(rows),encoding='utf-8')
 env=os.environ.copy();env.update(DSH_HOME=str(root/'home'),DSH_AGENTS_HOME=str(root/'agents'),DSH_TELEMETRY_DISABLED='1',SS_NATIVE_LAUNCHER=args.launcher,SS_NATIVE_PATCH=str(patch))
 env['NODE_OPTIONS']='--require="'+str(repo/'providers/acp/assets/stdio-console.cjs').replace('\\','/')+'"'
 env['SHISHI_DSH_ORIGINAL_NODE_OPTIONS']=os.environ.get('NODE_OPTIONS','')
 if any('"' in env[k] or '\n' in env[k] for k in ['SS_NATIVE_LAUNCHER','SS_NATIVE_PATCH']):raise RuntimeError('Invalid launcher or output path')
 stderr=(root/'stderr.txt').open('wb')
 child=subprocess.Popen('cmd.exe /D /Q /V:OFF /S /C ""%SS_NATIVE_LAUNCHER%" "--profile" "acp" "--patch" "%SS_NATIVE_PATCH%""',stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=stderr,env=env,creationflags=subprocess.CREATE_NO_WINDOW)
 messages=queue.Queue()
 def read():
  for line in child.stdout:messages.put(line)
 threading.Thread(target=read,daemon=True).start()
 def call(i,method,params):
  child.stdin.write((json.dumps({'jsonrpc':'2.0','id':i,'method':method,'params':params})+'\n').encode());child.stdin.flush();deadline=time.monotonic()+40
  while time.monotonic()<deadline:
   row=json.loads(messages.get(timeout=max(0.1,deadline-time.monotonic())))
   if row.get('id')==i:
    if 'error' in row:raise RuntimeError(str(row['error']))
    return row['result']
  raise RuntimeError('ACP handshake deadline')
 try:
  call(1,'initialize',{'protocolVersion':1,'clientCapabilities':{}})
  session=call(2,'session/new',{'cwd':str(work),'mcpServers':[]})['sessionId']
  call(3,'session/close',{'sessionId':session})
  (links/(session+'.json')).write_text(json.dumps({'version':1,'session_id':session}),encoding='utf-8')
  deadline=time.monotonic()+20
  while time.monotonic()<deadline:
   snapshot=links/'.workspaces.json'
   if snapshot.exists():
    projects=json.loads(snapshot.read_text(encoding='utf-8'))['projects']
    if any(Path(p['cwd']).resolve()==work.resolve() for p in projects):
     stored=[p for p in (root/'home/storages').rglob('*') if p.is_file() and p.stat().st_size<2_000_000]
     if any(session in p.read_text(encoding='utf-8',errors='replace') for p in stored):
      (root/'result.json').write_text(json.dumps({'passed':True,'native_workspace':True,'native_session_membership':True,'model_calls':0}),encoding='utf-8');print('PASS native DSH session persistence and workspace attachment; zero model calls');return
   time.sleep(.3)
  raise RuntimeError('Native workspace attachment was not observed')
 finally:
  child.stdin.close()
  try:child.wait(timeout=5)
  except subprocess.TimeoutExpired:subprocess.run(['taskkill','/PID',str(child.pid),'/T','/F'],capture_output=True)
  stderr.close()
if __name__=='__main__':main()
