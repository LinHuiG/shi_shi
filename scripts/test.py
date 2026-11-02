"""Offline test entry point. Focus local changes; keep full regression at delivery."""
from pathlib import Path
import argparse
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
UI = sorted(p.name for p in (ROOT / 'tests').glob('*.cjs')
            if (p.name.endswith('-ui.cjs') or p.name == 'ui-regression.cjs') and p.name != 'run-ui.cjs')
# Only leaf UI modules have a narrow map. Shared layout/state or unknown inputs
# deliberately fall back to the full set instead of silently omitting coverage.
# Explicit dependency groups. A known leaf runs its consumers; unknown/shared
# files still use the full plan. Filters are OR-ed by Rust's own test harness.
ANALYSIS = ('ai::', 'ai_backend::', 'ai_control::', 'analysis_context::', 'context_budget::',
            'engine::', 'archive::', 'snapshots::', 'wire_context::', 'tool_calling::', 'images::')
RUST_GROUPS = {
    **{name: ANALYSIS for name in ('ai.rs', 'analysis_context.rs', 'context_budget.rs',
                                  'snapshots.rs', 'wire_context.rs', 'archive.rs')},
    'credentials.rs': ('credentials::', 'ai_connections::', 'ai_backend::'),
    'calendar_service.rs': ('calendar_service::', 'holiday::', 'work_hours::', 'work_scheduler::'),
    'holiday.rs': ('calendar_service::', 'holiday::', 'work_hours::', 'work_scheduler::'),
    'schedule.rs': ('schedule::', 'engine::', 'work_scheduler::'),
    'message_progress.rs': ('message_progress::', 'context_budget::', 'engine::analysis_round_tests::'),
    'task_history.rs': ('task_history::', 'engine::task_status_tests::', 'ui_refresh::'),
    'images.rs': ('images::', 'analysis_context::', 'ai_backend::', 'engine::reply_context_tests::'),
    'tool_registry.rs': ('tool_registry::', 'tool_calling::', 'engine::workflow_tools_tests::'),
    'tool_runner.rs': ('tool_runner::', 'tool_calling::', 'engine::workflow_tools_tests::'),
    'engine/reports.rs': ('engine::report_status_tests::', 'engine::regression_tests::',
                          'calendar_service::', 'work_scheduler::', 'archive::', 'context_budget::'),
}

def rust_command(package, filters=()):
    command = ['cargo', 'test', '-p', package, '--locked', '--offline']
    if filters:
        command += ['--', *sorted(filters)]
    return command


UI_GROUPS = {
    'tags.js': ('tags', 'unified-tags', 'pending-fixes', 'workflow', 'task-experience'),
    'logs.js': ('logs', 'log-purpose', 'about-history', 'scoped-runtime', 'work-state', 'analysis-display'),
    'palette.js': ('palette', 'scoped-runtime', 'task-notification-navigation'),
}


def full_plan():
    return [['cargo', 'fmt', '--all', '--', '--check'],
            ['cargo', 'test', '--workspace', '--locked', '--offline'],
            ['node', 'tests/run-ui.cjs'], ['node', 'tests/dsh-workspace-bridge.cjs'],
            [sys.executable, 'tests/test-selection.py'],
            [sys.executable, 'tests/check-release.py'],
            [sys.executable, 'scripts/check_release.py']]


def changed_plan(paths):
    rust, ui, checks, bridge = {}, set(), set(), False

    def package(name, filters=()):
        if name in rust and rust[name] is None:
            return
        if not filters:
            rust[name] = None
        else:
            rust.setdefault(name, set()).update(filters)

    for name in paths:
        path = Path(name)
        if name.startswith('src/'):
            if name.startswith('src/ai_backend/'):
                package('shishi', ANALYSIS + ('handling::',))
            elif path.relative_to('src').as_posix() in RUST_GROUPS:
                package('shishi', RUST_GROUPS[path.relative_to('src').as_posix()])
            else:
                package('shishi')
                if not (path.stem.endswith('_tests') or path.name in ('tests.rs', 'test_support.rs')):
                    ui.update(UI)
        elif name.startswith(('providers/api/src/', 'providers/api/tests/')) and path.suffix == '.rs':
            package('shishi-api')
            if name.startswith('providers/api/src/'):
                package('shishi', ANALYSIS + ('ai_connections::', 'calendar_service::', 'holiday::'))
                if path.name == 'schema_validate.rs':
                    package('shishi-acp')
        elif name.startswith(('providers/acp/src/', 'providers/acp/tests/')) and path.suffix == '.rs':
            package('shishi-acp')
            if name.startswith('providers/acp/src/'):
                package('shishi', ANALYSIS + ('handling::',))
        elif name.startswith(('providers/', 'plugin-sdk/', 'examples/')):
            return full_plan()
        elif name.startswith('assets/ui/'):
            group = UI_GROUPS.get(path.name) if path.parent.as_posix() == 'assets/ui/scripts' else None
            ui.update((stem + '-ui.cjs' for stem in group) if group else UI)
        elif name.startswith('tests/') and path.name in UI:
            ui.add(path.name)
        elif name == 'tests/dsh-workspace-bridge.cjs':
            bridge = True
        elif name.startswith('docs/skills/') or name in ('docs/PLUGIN_ADAPTATION.md', 'PLUGIN_API.txt'):
            return full_plan()
        elif name.startswith('docs/') or (path.parent == Path('.') and path.suffix in ('.md', '.txt')):
            checks.add('scripts/check_release.py')
        else:
            return full_plan()
    plan = [rust_command(name, filters or ()) for name, filters in sorted(rust.items())]
    if ui:
        plan.append(['node', 'tests/run-ui.cjs', *sorted(ui)])
    if bridge:
        plan.append(['node', 'tests/dsh-workspace-bridge.cjs'])
    plan.extend([sys.executable, check] for check in sorted(checks))
    return plan


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', nargs='?', choices=('changed', 'full', 'rust', 'ui'), default='changed')
    parser.add_argument('filters', nargs='*', help='rust: one test filter; ui: suite filenames')
    parser.add_argument('--base', default='HEAD', help='compare tracked changes with this Git revision')
    parser.add_argument('--list', action='store_true', help='print commands without running them')
    args = parser.parse_args()
    if args.mode == 'changed':
        if args.filters:
            parser.error('changed accepts --base, not test filters')
        changed = subprocess.check_output(['git', 'diff', '--name-only', '-z', args.base, '--'], cwd=ROOT)
        added = subprocess.check_output(['git', 'ls-files', '--others', '--exclude-standard', '-z'], cwd=ROOT)
        plan = changed_plan(set((changed + added).decode('utf-8').split('\0')) - {''})
    elif args.mode == 'rust':
        if len(args.filters) != 1:
            parser.error('rust requires one explicit test filter, e.g. backlog::')
        plan = [['cargo', 'test', '-p', 'shishi', '--locked', '--offline', args.filters[0]]]
    elif args.mode == 'ui':
        if any(Path(name).name not in UI for name in args.filters):
            parser.error('unknown UI suite')
        plan = [['node', 'tests/run-ui.cjs', *(Path(name).name for name in args.filters)]]
    else:
        if args.filters:
            parser.error('full does not accept filters')
        plan = full_plan()
    if not plan:
        print('No changed files. Use --base HEAD~1 for the last commit, or full for delivery.')
    for command in plan:
        print('+ ' + subprocess.list2cmdline(command), flush=True)
        if not args.list:
            subprocess.run(command, cwd=ROOT, check=True)


if __name__ == '__main__':
    main()
