"""Guard the runner's fail-closed selection: shared/unknown changes run fully."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('test_plan', Path(__file__).resolve().parents[1] / 'scripts/test.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class Selection(unittest.TestCase):
    def test_unknown_shared_and_provider_changes_cannot_skip_full_regression(self):
        for path in ('Cargo.toml', 'providers/api/Cargo.toml', 'tests/ui-driver.cjs',
                     'tests/removed-ui.cjs', 'config/prompts.json', 'scripts/test.py',
                     'assets/new-prompt.txt', 'tests/fixture.md', 'docs/skills/shishi-cli/SKILL.md',
                     'docs/PLUGIN_ADAPTATION.md', 'PLUGIN_API.txt'):
            with self.subTest(path=path):
                self.assertEqual(runner.changed_plan([path]), runner.full_plan())

    def test_leaf_ui_and_its_test_are_deduplicated(self):
        plan = runner.changed_plan(['assets/ui/scripts/tags.js', 'tests/tags-ui.cjs'])
        self.assertEqual(len(plan), 1)
        self.assertEqual(plan[0].count('tags-ui.cjs'), 1)
        self.assertIn('unified-tags-ui.cjs', plan[0])
        self.assertLess(len(plan[0]) - 2, len(runner.UI))

    def test_rust_ipc_and_shared_ui_keep_all_ui_coverage(self):
        for path in ('src/engine.rs', 'assets/ui/styles/base.css', 'assets/ui/scripts/core.js'):
            with self.subTest(path=path):
                plan = runner.changed_plan([path])
                ui = next(command for command in plan if command[:2] == ['node', 'tests/run-ui.cjs'])
                self.assertEqual(set(ui[2:]), set(runner.UI))

    def test_rust_leaf_runs_consumers_without_unrelated_browser_suites(self):
        plan = runner.changed_plan(['src/credentials.rs', 'src/ai_backend/model_api_tests.rs'])
        self.assertEqual(len(plan), 1)
        self.assertIn('credentials::', plan[0])
        self.assertIn('ai_backend::', plan[0])
        self.assertIn('handling::', plan[0])
        self.assertEqual(plan[0].count('ai_backend::'), 1)
        broad = runner.changed_plan(['src/credentials.rs', 'src/model.rs'])
        self.assertEqual(broad[0], runner.rust_command('shishi'))
        self.assertTrue(any(command[0] == 'node' for command in broad))
        reports = runner.changed_plan(['src/engine/reports.rs'])
        self.assertEqual(len(reports), 1)
        for consumer in ('engine::report_status_tests::', 'engine::regression_tests::',
                         'archive::', 'calendar_service::', 'work_scheduler::', 'context_budget::'):
            self.assertIn(consumer, reports[0])
        self.assertEqual(runner.changed_plan(['src/engine/reports.rs', 'src/engine.rs'])[0],
                         runner.rust_command('shishi'))

    def test_provider_contracts_remain_selected(self):
        schema = runner.changed_plan(['providers/api/src/schema_validate.rs'])
        self.assertEqual({command[3] for command in schema}, {'shishi', 'shishi-api', 'shishi-acp'})
        transport = runner.changed_plan(['providers/api/src/http_client.rs'])
        self.assertEqual({command[3] for command in transport}, {'shishi', 'shishi-api'})
        isolated = runner.changed_plan(['providers/api/tests/openai_mock.rs'])
        self.assertEqual(isolated, [runner.rust_command('shishi-api')])

    def test_clean_tree_and_documentation_do_not_start_browsers(self):
        self.assertEqual(runner.changed_plan([]), [])
        plan = runner.changed_plan(['docs/DEVELOPMENT.md'])
        self.assertEqual(len(plan), 1)
        self.assertEqual(plan[0][-1], 'scripts/check_release.py')


if __name__ == '__main__':
    unittest.main()
