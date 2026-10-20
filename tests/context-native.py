"""No-model native MCP smoke. Uses only a fresh synthetic root and Codex home."""
import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import time
import uuid


class Rpc:
    def __init__(self, args, output, env=None):
        self.stderr = open(output, 'w', encoding='utf-8')
        self.process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.stderr, text=True, encoding='utf-8',
                                        env=env, creationflags=subprocess.CREATE_NO_WINDOW)
        self.rows = queue.Queue()
        self.seq = 0
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.process.stdout:
            try:
                self.rows.put(json.loads(line))
            except ValueError:
                pass

    def call(self, method, params=None):
        self.seq += 1
        self.process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': self.seq,
                                            'method': method, 'params': params or {}}) + '\n')
        self.process.stdin.flush()
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            row = self.rows.get(timeout=max(.01, deadline-time.monotonic()))
            if row.get('id') == self.seq:
                if 'error' in row:
                    raise RuntimeError(f'{method}: {row["error"]}')
                return row['result']
        raise TimeoutError(method)

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.stderr.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = args.output.resolve() / ('context-' + str(uuid.uuid4()))
    context_dir = root / 'data' / 'handling-context'
    context_dir.mkdir(parents=True)
    state = {'tasks': [{'id': 'task-a', 'title': 'Synthetic A'},
                       {'id': 'task-b', 'title': 'Synthetic B'}], 'messages': {}}
    (root/'data'/'state.json').write_text(json.dumps(state), encoding='utf-8')
    tokens = [str(uuid.uuid4()), str(uuid.uuid4())]
    for token, task in zip(tokens, ['task-a', 'task-b']):
        ticket = {'root': str(root), 'run': token, 'task': task,
                  'expires': int(time.time()*1000)+60000, 'scopes': []}
        (context_dir/(token+'.json')).write_text(json.dumps(ticket), encoding='utf-8')
    for attempt in range(2):
        service = Rpc([str(args.binary.resolve()), '--context-mcp-root', str(root)],
                      root/f'service-{attempt}.log')
        try:
            service.call('initialize', {'protocolVersion': '2024-11-05',
                                       'capabilities': {}, 'clientInfo': {'name': 'synthetic', 'version': '1'}})
            tools = service.call('tools/list')['tools']
            assert all('context_token' in t['inputSchema']['required'] for t in tools)
            for token, title in zip(tokens, ['Synthetic A', 'Synthetic B']):
                result = service.call('tools/call', {'name': 'get_task_context',
                                                     'arguments': {'context_token': token}})
                assert json.loads(result['content'][0]['text'])['task']['title'] == title
            for value in [{}, {'context_token': '../state'}, {'context_token': str(uuid.uuid4())}]:
                assert service.call('tools/call', {'name': 'get_task_context', 'arguments': value})['isError']
        finally:
            service.close()
    # A separate Codex process discovers the same native server, with no SS
    # process and no account, user configuration, conversation or model call.
    codex_home = root/'isolated-codex-home'
    codex_home.mkdir()
    env = os.environ.copy()
    env['CODEX_HOME'] = str(codex_home)
    server = Rpc([str(args.codex.resolve()), 'app-server'], root/'codex.log', env)
    try:
        server.call('initialize', {'clientInfo': {'name': 'shishi_context_smoke', 'version': '1'},
                                  'capabilities': {'experimentalApi': True}})
        server.call('config/value/write', {'keyPath': 'mcp_servers.shishi_context',
                    'mergeStrategy': 'upsert', 'value': {'command': str(args.binary.resolve()),
                    'args': ['--context-mcp-root', str(root)], 'enabled': True}})
        server.process.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'initialized'})+'\n')
        server.process.stdin.flush()
        server.call('config/mcpServer/reload')
        inventory = server.call('mcpServerStatus/list', {'serverName': 'shishi_context',
                                                       'detail': 'toolsAndAuthOnly'})
        rows = [r for r in inventory['data'] if r['name'] == 'shishi_context']
        assert rows and 'get_task_context' in rows[0]['tools'], inventory
    finally:
        server.close()
    (root/'result.json').write_text(json.dumps({'ok': True, 'model_turns': 0,
        'restarts': 1, 'task_scopes': 2, 'codex_inventory': True}), encoding='utf-8')
    print('PASS native MCP restart, explicit task isolation, invalid tokens, and isolated Codex discovery; '+str(root))


if __name__ == '__main__':
    main()
