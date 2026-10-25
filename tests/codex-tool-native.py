"""Real Codex CLI as a plain model; all model responses come from loopback."""
import argparse
import http.server
import json
import os
from pathlib import Path
import subprocess
import threading
import uuid


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--codex',type=Path,required=True)
    parser.add_argument('--model',required=True,help='CLI model metadata only; model transport is synthetic')
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    root=args.output.resolve()/('codex-text-'+uuid.uuid4().hex)
    root.mkdir(parents=True)
    requests=[]

    class Model(http.server.BaseHTTPRequestHandler):
        def log_message(self,*values):pass
        def do_POST(self):
            length=int(self.headers['Content-Length'])
            if length>2*1024*1024:self.send_error(413);return
            requests.append(json.loads(self.rfile.read(length)))
            value=({'tool_call':{'name':'ss_echo','arguments':json.dumps({'text':'SS_TOOL_OK'})},'result':None}
                   if len(requests)==1 else {'tool_call':None,'result':{'answer':'SS_TOOL_OK'}})
            text=json.dumps(value)
            item={'id':'synthetic-message','type':'message','role':'assistant','status':'completed',
                  'content':[{'type':'output_text','text':text,'annotations':[]}]}
            events=[('response.output_item.added',{'output_index':0,'item':{**item,'content':[],'status':'in_progress'}}),
                    ('response.output_text.delta',{'item_id':item['id'],'output_index':0,'content_index':0,'delta':text}),
                    ('response.output_item.done',{'output_index':0,'item':item}),
                    ('response.completed',{'response':{'id':'synthetic-response','status':'completed','output':[item],
                     'usage':{'input_tokens':1,'output_tokens':1,'total_tokens':2}}})]
            self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
            for name,value in events:
                self.wfile.write(('event: '+name+'\ndata: '+json.dumps({'type':name,**value})+'\n\n').encode())
                self.wfile.flush()

    with http.server.HTTPServer(('127.0.0.1',0),Model) as server:
        threading.Thread(target=server.serve_forever,daemon=True).start()
        try:
            source=root/'wrapper.rs'
            source.write_text(r"""use std::{fs,process::{Command,Stdio},os::windows::process::CommandExt};
fn main(){let path=std::env::current_exe().unwrap().with_extension("settings");let text=fs::read_to_string(path).unwrap();let mut lines=text.lines();let mut command=Command::new(lines.next().unwrap());for line in lines {command.arg("-c").arg(line);}command.args(std::env::args().skip(1)).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).creation_flags(0x08000000);let status=command.status().unwrap();std::process::exit(status.code().unwrap_or(1));}
""",encoding='utf-8')
            wrapper=root/'synthetic-provider.exe'
            subprocess.run(['rustc','--edition=2021',str(source),'-o',str(wrapper)],check=True,capture_output=True,
                           creationflags=subprocess.CREATE_NO_WINDOW)
            assert '\n' not in str(args.codex) and '\r' not in str(args.codex)
            wrapper.with_suffix('.settings').write_text(str(args.codex.resolve())+'\nmodel_provider="synthetic"\n'
                +'model_providers.synthetic={name="Synthetic local model",base_url="http://127.0.0.1:'+str(server.server_port)
                +'/v1",wire_api="responses",request_max_retries=0,stream_max_retries=0,requires_openai_auth=false,supports_websockets=false}\n',encoding='utf-8')
            config_root=root/'config-fixture';(config_root/'data').mkdir(parents=True)
            state={'schema_version':1,'config':{'codex':str(wrapper),'codex_models':[{'model':args.model,'supportedReasoningEfforts':[{'reasoningEffort':'low'}]}],
                    'enabled':False,'auto_ai':False,'auto_update':False},'tasks':[],'messages':{},'analysis_memory':{},'tag_state':{},'audit':[],'suggestions':[],
                    'work_scheduler':{'schema_version':1,'runtime':{'schema_version':1,'active_overtime':None,'day_records':{}},'last_schedule':None,'clock_floor_ms':None}}
            (config_root/'data/state.json').write_text(json.dumps(state),encoding='utf-8')
            home=root/'isolated-home';home.mkdir()
            env={k:v for k,v in os.environ.items() if not k.startswith('CODEX_') and k!='OPENAI_API_KEY'}
            env['CODEX_HOME']=str(home)
            output=root/'ss-probe'
            command=[str(args.binary.resolve()),'--live-ai-regression','--allow-live-ai','--config-root',str(config_root),
                     '--root',str(output),'--only-source','codex','--only-scenario','tool_call']
            result=subprocess.run(command,env=env,capture_output=True,timeout=45,creationflags=subprocess.CREATE_NO_WINDOW)
            report=json.loads((output/'report.json').read_text(encoding='utf-8-sig'))
            assert result.returncode==0,report
            assert len(requests)==2,len(requests)
            row=report['rows'][0];assert row['status']=='passed' and row['detail']['ss_executed'] is True
            assert row['detail']['stdout'].strip()=='"SS_TOOL_OK"'
            def tool_names(tool):
                return [tool.get('name','')]+[name for child in tool.get('tools',[]) for name in tool_names(child)]
            for request in requests:
                names=[name for item in request.get('input',[]) if item.get('type')=='additional_tools'
                       for tool in item.get('tools',[]) for name in tool_names(tool)]
                assert not any(name in ('ss_echo','ss_tool') or name.startswith('mcp__shishi_') for name in names),names
            transcripts=[]
            for item in requests[-1]['input']:
                for part in item.get('content',[]) if isinstance(item.get('content'),list) else []:
                    text=part.get('text','')
                    if '\n\n本次对话：' in text:transcripts=json.loads(text.split('\n\n本次对话：',1)[1])
            tool_messages=[message for message in transcripts if message.get('role')=='tool']
            assert len(tool_messages)==1,transcripts
            tool_result=json.loads(tool_messages[0]['content'])
            assert tool_result['status']=='completed' and tool_result['stdout'].strip()=='"SS_TOOL_OK"'
            summary={'ok':True,'real_model_calls':0,'model_responses':'synthetic loopback',
                     'text_tool_request_parsed_by_ss':True,'command_executed_in_ss':True,
                     'result_returned_to_model':True,'registered_mcp_tools_in_model':False}
            (root/'report.json').write_text(json.dumps(summary,indent=2),encoding='utf-8')
            print('PASS native Codex text request -> SS execution -> model continuation, no model MCP; '+str(root))
        finally:server.shutdown()


if __name__=='__main__':main()
