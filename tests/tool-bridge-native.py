"""No-model check of the generic handling MCP entry and its SS owner callback."""
import argparse
import json
from pathlib import Path
import runpy
import socketserver
import threading
import time
import uuid

Rpc=runpy.run_path(str(Path(__file__).with_name('context-native.py')))['Rpc']


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    root=args.output.resolve()/('tool-bridge-'+str(uuid.uuid4()))
    context_dir=root/'data/handling-context';context_dir.mkdir(parents=True)
    calls=[];token=str(uuid.uuid4());context_token=str(uuid.uuid4());name='synthetic_lookup'
    state={'schema_version':1,'tasks':[{'id':'task-a','title':'Synthetic task','tags':[],'evidence':{'direct':[],'background':[],'completion':[]}}],'messages':{},
           'config':{'enabled':False,'auto_ai':False,'auto_update':False,'tools':[{'definition_version':2,'id':'synthetic','name':name,
                     'description':'Synthetic lookup','command_template':'echo {{value}}','enabled':True,
                     'parameters':[{'name':'value','type':'string','description':'Synthetic string'}]}]},
           'analysis_memory':{},'tag_state':{},'audit':[],'suggestions':[],
           'work_scheduler':{'schema_version':1,'runtime':{'schema_version':1,'active_overtime':None,'day_records':{}},
                             'last_schedule':None,'clock_floor_ms':None}}
    (root/'data/state.json').write_text(json.dumps(state),encoding='utf-8')
    ticket={'root':str(root),'task':'task-a','run':context_token,'expires':int(time.time()*1000)+60000,'scopes':[]}
    (context_dir/(context_token+'.json')).write_text(json.dumps(ticket),encoding='utf-8')

    class Parent(socketserver.StreamRequestHandler):
        def handle(self):
            value=json.loads(self.rfile.readline(128*1024))
            assert value['token']==token and value['context_token']==context_token
            calls.append(value)
            result={'tool':value['name'],'call_id':value['call_id'],'status':'completed','exit_code':0,
                    'stdout':'Synthetic callback result','stderr':''}
            self.wfile.write((json.dumps({'content':[{'type':'text','text':json.dumps(result)}]})+'\n').encode())

    with socketserver.TCPServer(('127.0.0.1',0),Parent) as parent:
        worker=threading.Thread(target=parent.serve_forever,daemon=True);worker.start()
        route={'address':f'127.0.0.1:{parent.server_address[1]}','token':token}
        (context_dir/'tool-service.json').write_text(json.dumps(route),encoding='utf-8')
        child=Rpc([str(args.binary.resolve()),'--context-mcp-root',str(root)],root/'stderr.log')
        try:
            child.call('initialize')
            tools=child.call('tools/list')['tools'];names=[tool['name'] for tool in tools]
            assert names.count('ss_tool')==1 and name not in names
            schema=next(tool['inputSchema'] for tool in tools if tool['name']=='ss_tool')
            assert set(schema['properties'])=={'context_token','tool_name','arguments'}
            assert set(schema['required'])=={'context_token','tool_name','arguments'}
            context=child.call('tools/call',{'name':'get_task_context','arguments':{'context_token':context_token}})
            assert not context.get('isError'),context
            value=json.loads(context['content'][0]['text'])
            assert value['available_tools'][0]['name']==name
            assert 'command_template' not in value['available_tools'][0]
            assert child.call('tools/call',{'name':name,'arguments':{'context_token':context_token}})['isError']
            assert child.call('tools/call',{'name':'ss_tool','arguments':{'tool_name':name,'arguments':{}}})['isError']
            assert calls==[]
            response=child.call('tools/call',{'name':'ss_tool','arguments':{'context_token':context_token,
                                     'tool_name':name,'arguments':{'value':'合成参数'}}})
            result=json.loads(response['content'][0]['text'])
            assert result['stdout']=='Synthetic callback result'
            assert len(calls)==1 and calls[0]['name']==name and calls[0]['arguments']=={'value':'合成参数'}
            assert calls[0]['call_id'].startswith('mcp:') and calls[0]['call_id'].endswith(':'+str(child.seq))
            assert child.call('ping')=={}
        finally:
            child.close();parent.shutdown();worker.join(timeout=5)
    report={'ok':True,'one_generic_mcp_function':True,'business_names_not_mcp_functions':True,
            'existing_task_credential':True,'ss_owner_callback_correlated':True,'real_model_calls':0,'business_commands':0}
    (root/'report.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
    print('PASS native ss_tool, existing task credential, dynamic catalogue and SS owner callback; '+str(root))


if __name__=='__main__':main()
