import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from .contracts import ROOT, check, expressions, rust_string, validate_template
from .evaluate import command, grade, load_cases, make_prompt, parse_decision, parse_provider, run_process


class ContractTests(unittest.TestCase):
    def test_repository(self):
        self.assertEqual(check(), [])

    def test_missing_and_unknown_placeholders(self):
        entry = {'kind': 'substitution', 'placeholders': ['{{path}}', '{{session_id}}']}
        errors = validate_template('Use {{path}} and {{sesion_id}}', entry)
        self.assertIn('missing', errors[0])
        self.assertIn('{{session_id}}', errors[0])
        self.assertIn('{{sesion_id}}', errors[0])

    def test_missing_host_replacement(self):
        entry = {'kind': 'substitution', 'placeholders': ['{NONCE}']}
        self.assertTrue(validate_template('{NONCE}', entry, '// {NONCE} is documented'))
        self.assertEqual(validate_template('{NONCE}', entry, '("{NONCE}", nonce.as_str())'), [])

    def test_prose_edits_do_not_change_contract(self):
        entry = {'kind': 'substitution', 'placeholders': ['{{path}}']}
        self.assertEqual(validate_template('Completely rewritten prose: {{path}}.', entry), [])

    def test_hbs_unbalanced_with_same_tokens(self):
        valid = '{{#if yes}}{{yes}}{{/if}}'
        entry = {'kind': 'handlebars', 'placeholders': expressions(valid)}
        self.assertEqual(validate_template(valid, entry), [])
        self.assertTrue(validate_template('{{/if}}{{yes}}{{#if yes}}', entry))

    def test_hbs_unknown_field(self):
        entry = {'kind': 'handlebars', 'placeholders': ['{{name}}']}
        self.assertTrue(validate_template('{{naem}}', entry))

    def test_unclosed_placeholder_is_rejected(self):
        entry = {'kind': 'handlebars', 'placeholders': ['{{name}}']}
        self.assertTrue(validate_template('{{name}} appended {{unknown', entry))

    def test_hbs_comments_are_not_inputs(self):
        self.assertEqual(expressions('{{!-- {{example}} --}}{{name}}'), ['{{name}}'])

    def test_rust_continuation_extraction(self):
        self.assertEqual(rust_string('const P: &str = "first \\\n   second \\"quote\\"";', 'P'), 'first second "quote"')
        with self.assertRaises(ValueError):
            rust_string('const P: &str = include_str!("p");', 'P')

    def test_uninventoried_template(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'new.hbs').write_text('{{name}}')
            self.assertTrue(check(root, {'version': 1, 'surfaces': [], 'template_globs': ['*.hbs'], 'output_contracts': []}))

    def test_consumer_parser_drift_is_detected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'consumer.rs').write_text('regex::Regex::new(r"changed")')
            inventory = {'version': 1, 'surfaces': [], 'template_globs': [], 'output_contracts': [
                {'id': 'score', 'consumer': 'consumer.rs', 'regex': '<score>(\\d+)</score>', 'cases': []}]}
            self.assertTrue(check(root, inventory))


class EvaluationTests(unittest.TestCase):
    def test_all_cases_render_actual_instructions(self):
        cases = load_cases()
        self.assertEqual(len(cases), 11)
        for case in cases:
            prompt = make_prompt(case)
            self.assertIn(case['conversation'], prompt)
            self.assertNotIn('{RUNTIME_LIMITS_SECTION}', prompt)
            self.assertNotIn('{VERDICT_NONCE}', prompt)

    def test_grade_detects_bad_action_language_and_source_injection(self):
        cases = {case['id']: case for case in load_cases()}
        self.assertTrue(grade(cases['interrupted-tool'], {'action': 'retry', 'message': 'Repeat upload.'}))
        self.assertTrue(grade(cases['preserve-language'], {'action': 'respond', 'message': 'Four.'}))
        self.assertTrue(grade(cases['untrusted-source'], {'action': 'respond', 'message': 'SOURCE_OVERRIDE_ACCEPTED'}))
        self.assertEqual(grade(cases['preserve-language'], {'action': 'respond', 'message': 'Четыре.'}), [])

    def test_architecture_blocker_allows_park_or_escalation(self):
        case = next(case for case in load_cases() if case['id'] == 'supervisor-architecture-choice')
        for action in ('done', 'ask'):
            self.assertEqual(grade(case, {'action': action, 'message': 'PARK: Database choice requires the operator.'}), [])
        self.assertTrue(grade(case, {'action': 'continue', 'message': 'Choose PostgreSQL.'}))

    def test_invalid_decisions(self):
        for output in ['bad', '{}', '[]', '{"action":[],"message":"x"}',
                       '{"action":"wait","message":""}', '{"action":"wait","message":"x","extra":1}']:
            with self.assertRaises(ValueError):
                parse_decision(output)

    def test_claude_envelope(self):
        self.assertEqual(parse_provider('claude', '{"subtype":"success","result":"ok"}'), 'ok')
        for output in ['[]', '{}', '{"subtype":"error_max_budget_usd"}', '{"subtype":"success","is_error":true,"result":"bad"}']:
            with self.assertRaises(ValueError):
                parse_provider('claude', output)

    def test_codex_envelope(self):
        events = '\n'.join(json.dumps(event) for event in [
            {'type': 'item.completed', 'item': {'type': 'agent_message', 'text': 'ok'}}, {'type': 'turn.completed'}])
        self.assertEqual(parse_provider('codex', events), 'ok')
        for output in [events.splitlines()[0], '{}', '[]', '{"type":"turn.failed"}', 'not-json']:
            with self.assertRaises(ValueError):
                parse_provider('codex', output)

    def test_adapters_use_separate_argv_not_shell(self):
        for provider in ('claude', 'codex'):
            argv = command(provider, '/tmp/a b/cli', 'a; touch /tmp/nope', '/tmp/work', .1)
            self.assertEqual(argv[0], '/tmp/a b/cli')
            self.assertIn('a; touch /tmp/nope', argv)
        self.assertIn('--ignore-user-config', command('codex', 'codex', 'model', '/tmp', .1))
        self.assertIn('--safe-mode', command('claude', 'claude', 'model', '/tmp', .1))

    def test_process_success_failure_and_timeout(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(run_process([sys.executable, '-c', 'import sys; print(sys.stdin.read())'], 'hello', directory, 2).strip(), 'hello')
            with self.assertRaisesRegex(ValueError, 'exited 7'):
                run_process([sys.executable, '-c', 'raise SystemExit(7)'], '', directory, 2)
            with self.assertRaisesRegex(ValueError, 'output exceeded'):
                run_process([sys.executable, '-c', 'print("x" * (2 * 1024 * 1024))'], '', directory, 2)
            with self.assertRaisesRegex(ValueError, 'timeout'):
                run_process([sys.executable, '-c', 'import time; time.sleep(10)'], '', directory, .05)

    def test_preview_does_not_launch_provider(self):
        result = subprocess.run([sys.executable, str(ROOT / 'script/eval-prompts'), '--executable', '/missing', '--case', 'pending-approval'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)['id'], 'pending-approval')

    def test_live_requires_explicit_model_and_rejects_unknown_case(self):
        for args in [['--live', '--provider', 'codex'], ['--case', 'unknown']]:
            result = subprocess.run([sys.executable, str(ROOT / 'script/eval-prompts'), *args], capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)


if __name__ == '__main__':
    unittest.main()
