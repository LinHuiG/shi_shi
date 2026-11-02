"""Exercise the real CLI and one isolated desktop owner; no real models or sources."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import ctypes
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import time
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--cli', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    cli = args.cli.resolve()
    root = args.output.resolve() / ('cli with spaces-' + str(uuid.uuid4()))
    parent = root.parent
    parent.mkdir(parents=True, exist_ok=True)
    owned = set()
    launchers = []
    checks = []

    def invoke(*words, payload=None, ok=True):
        command = [str(cli), '--root', str(root), '--no-background-jobs', *words]
        encoded = None
        if payload is not None:
            command += ['--input', '-']
            encoded = json.dumps(payload, ensure_ascii=False).encode('utf-8')
        completed = subprocess.run(command, input=encoded, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, timeout=80)
        value = json.loads(completed.stdout.decode('utf-8-sig'))
        assert value['ok'] is ok, (words, value, completed.stderr.decode('utf-8', 'replace'))
        assert completed.returncode == (0 if ok else 1), (words, completed.returncode)
        result = value.get('data', value)
        if isinstance(result, dict) and result.get('pid'):
            owned.add(result['pid'])
        print('checked ' + ' '.join(words[:2]), flush=True)
        return result

    def alive(pid):
        kernel = ctypes.windll.kernel32
        kernel.OpenProcess.restype = ctypes.c_void_p
        handle = kernel.OpenProcess(0x00100000, False, pid)
        if not handle:
            return False
        try:
            return kernel.WaitForSingleObject(ctypes.c_void_p(handle), 0) == 258
        finally:
            kernel.CloseHandle(ctypes.c_void_p(handle))

    def wait_stopped(pid):
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline and alive(pid):
            time.sleep(.1)
        assert not alive(pid), f'Isolated backend {pid} did not stop'

    def open_gui():
        process = subprocess.Popen([str(cli.with_name('shishi.exe')), '--root', str(root),
                                    '--no-background-jobs', '--no-auto-update'],
                                   stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, creationflags=subprocess.CREATE_NO_WINDOW)
        launchers.append(process)
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            status = invoke('backend', 'status')
            if status.get('gui'):
                return status
            time.sleep(.1)
        raise AssertionError('Isolated GUI did not acquire its owner flag')

    try:
        assert invoke('backend', 'status')['running'] is False
        assert not root.exists(), 'Status must not initialize an installation'
        with ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(invoke, 'backend', 'start') for _ in range(2)]
            starts = [f.result() for f in futures]
        assert starts[0]['pid'] == starts[1]['pid']
        assert starts[0]['cli'] and not starts[0]['gui']
        pid = starts[0]['pid']
        checks += ['status_without_start', 'simultaneous_single_owner', 'cli_only_start']

        methods = invoke('methods')['methods']
        assert all(m in methods for m in ['edit', 'memory_save', 'source_save', 'schedule_edit',
                                         'connection_save', 'tool_save', 'storage_change', 'cli_skill'])
        skill = invoke('call', 'cli_skill')['text']
        assert str(cli).replace("'", "''") in skill
        assert "--root '" + str(root).replace("'", "''") + "'" in skill
        assert 'name: shishi-cli' in skill and 'expected' in skill and 'job_id' in skill
        checks.append('skill_copy_targets_current_instance')
        task = invoke('call', 'edit', payload={'task': {
            'title': '合成事项 🙂', 'description': '第一行\n第二行 "引号" & | %', 'status': '待处理'}})
        read = invoke('tasks', 'get', task['id'])
        assert read['description'] == task['description']
        result = invoke('tasks', 'list', payload={'query': '合成事项', 'limit': 1, 'offset': 0})
        assert result['total'] == 1 and result['items'][0]['id'] == task['id']
        # Two real requests based on the same version: only one can commit.
        def edit_title(title):
            command = [str(cli), '--root', str(root), 'call', 'edit', '--input', '-']
            p = {'task': {'id': task['id'], 'updated': read['updated'], 'title': title}}
            out = subprocess.run(command, input=json.dumps(p).encode(), stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, timeout=30)
            return json.loads(out.stdout)
        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(edit_title, ['合成编辑一', '合成编辑二']))
        assert sorted(v['ok'] for v in results) == [False, True]
        current = invoke('tasks', 'get', task['id'])
        winner = next(v['data'] for v in results if v['ok'])
        assert current['title'] == winner['title'] and current['description'] == task['description']
        invoke('call', 'action', payload={'id': task['id'], 'action': 'done'})
        assert invoke('tasks', 'get', task['id'])['status'] == '已完成'
        checks += ['shared_dispatch_discovery', 'unicode_stdin', 'pagination', 'concurrent_stale_write_rejected']

        memory = invoke('memory', 'read')
        expected = memory['temporary']['revision']
        invoke('memory', 'save', payload={'kind': 'temporary', 'expected': expected, 'text': '合成记忆 🙂'})
        invoke('memory', 'save', payload={'kind': 'temporary', 'expected': expected, 'text': '旧草稿'}, ok=False)
        fixture = root / '归档 记忆.json'
        fixture.write_text(json.dumps({'kind': 'archived', 'expected': memory['archived']['revision'],
                                       'text': '合成长期记忆'},
                                      ensure_ascii=False), encoding='utf-8-sig')
        invoke('memory', 'save', '--input', str(fixture))
        invoke('call', 'not_a_method', ok=False)
        checks += ['memory_revisions', 'unicode_file_input', 'unknown_method']

        route = json.loads((root / 'data/local-service.json').read_text())
        address, port = route['address'].rsplit(':', 1)
        with socket.create_connection((address, int(port)), timeout=5) as connection:
            request = {'protocol': 1, 'token': 'invalid', 'id': 'unauthorized',
                       'method': 'memory_save', 'params': {'kind': 'temporary', 'expected': expected + 1,
                                                         'text': 'must not be written'}}
            connection.sendall(json.dumps(request).encode() + b'\n')
            response = json.loads(connection.makefile('rb').readline())
            assert response['ok'] is False
        assert invoke('memory', 'read')['temporary']['text'] == '合成记忆 🙂'
        checks += ['local_authentication']

        route_path = root / 'data/local-service.json'
        original_route = route_path.read_bytes()
        try:
            route_path.write_text(json.dumps({**route, 'protocol': 999}), encoding='utf-8')
            invoke('backend', 'status', ok=False)
            invoke('tasks', 'list', ok=False)
        finally:
            route_path.write_bytes(original_route)
        assert invoke('backend', 'status')['pid'] == pid
        checks += ['incompatible_protocol_is_not_reported_as_stopped']

        assert open_gui()['pid'] == pid
        status = invoke('backend', 'status')
        assert status['gui'] and status['cli']
        result = invoke('backend', 'stop')
        assert result['gui'] and not result['cli'] and not result['closing']
        assert alive(pid)
        # Close the remaining GUI owner without touching another installation.
        result = invoke('ui', 'close')
        assert result['closing']
        wait_stopped(pid)
        checks += ['noninteractive_default_keeps_gui', 'last_owner_stops']

        # An ordinary query starts and retains the backend after ss has exited.
        assert invoke('memory', 'read')['temporary']['text'] == '合成记忆 🙂'
        pid = invoke('backend', 'status')['pid']
        assert invoke('memory', 'read')['temporary']['text'] == '合成记忆 🙂'
        assert invoke('tasks', 'get', task['id'])['status'] == '已完成'
        invoke('ui', 'open')
        result = invoke('ui', 'close')
        assert not result['gui'] and result['cli'] and not result['closing']
        invoke('ui', 'open')
        assert invoke('backend', 'stop', '--force')['closing']
        wait_stopped(pid)
        checks += ['ordinary_query_auto_start', 'restart_preserves_data', 'default_keeps_cli', 'force_closes_both']

        status = open_gui()
        assert status['gui'] and not status['cli']
        assert invoke('ui', 'close')['closing']
        wait_stopped(status['pid'])
        with ThreadPoolExecutor(max_workers=2) as pool:
            gui = pool.submit(open_gui)
            backend = pool.submit(invoke, 'backend', 'start')
            assert gui.result()['pid'] == backend.result()['pid']
        status = invoke('backend', 'status')
        assert status['gui'] and status['cli']
        invoke('backend', 'stop', '--force')
        wait_stopped(status['pid'])
        checks += ['gui_only_start', 'simultaneous_gui_cli_start']

        invoke('backend', 'start')
        pid = invoke('backend', 'status')['pid']
        invoke('ui', 'open')
        # Fault injection targets only the PID returned by our unique fixture.
        os.kill(pid, signal.SIGTERM)
        wait_stopped(pid)
        result = invoke('backend', 'start')
        assert result['pid'] != pid and result['cli'] and not result['gui']
        assert invoke('memory', 'read')['archived']['text'] == '合成长期记忆'
        checks += ['stale_route_recovery', 'crash_does_not_restore_gui_owner']

        # Actual migrations restart the owner and must preserve both flags.
        for with_gui in [False, True]:
            if with_gui:
                invoke('ui', 'open')
            before = invoke('backend', 'status')
            destination = parent / (root.name + ('-both-data' if with_gui else '-cli-data'))
            destination.mkdir()
            invoke('call', 'storage_change', payload={'directory': str(destination), 'copy': True})
            wait_stopped(before['pid'])
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                status = invoke('backend', 'status')
                if status.get('running') and status['pid'] != before['pid']:
                    break
                time.sleep(.2)
            else:
                raise AssertionError('Migrated backend did not restart')
            assert status['cli'] and status['gui'] is with_gui
            assert invoke('tasks', 'get', task['id'])['status'] == '已完成'
            assert invoke('memory', 'read')['temporary']['text'] == '合成记忆 🙂'
            assert Path(json.loads((root / 'data-location.json').read_text())['directory']) == destination
        checks += ['migration_preserves_cli_only_owner', 'migration_preserves_both_owners']
        report = {'ok': True, 'checks': checks, 'real_model_calls': 0, 'root': str(root)}
        (root / 'report.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
        print(json.dumps(report, ensure_ascii=False))
    finally:
        try:
            invoke('backend', 'stop', '--force')
        except Exception:
            pass
        for pid in owned:
            if alive(pid):
                wait_stopped(pid)
        for process in launchers:
            process.wait(timeout=10)


if __name__ == '__main__':
    main()
