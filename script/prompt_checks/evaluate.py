"""Opt-in, synthetic prompt probes. Results are not proof of operational safety."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time

from .contracts import ROOT, rust_string

CASES = Path(__file__).with_name('cases.json')
ACTIONS = {'observe', 'retry', 'clarify', 'wait', 'respond', 'continue', 'done', 'ask_agent', 'ask', 'compact'}


def load_cases():
    data = json.loads(CASES.read_text())
    if data['version'] != 1:
        raise ValueError('unsupported behavioral case version')
    seen = set()
    for case in data['cases']:
        if case['id'] in seen or case['expected_action'] not in ACTIONS:
            raise ValueError('invalid or duplicate behavioral case')
        seen.add(case['id'])
    return data['cases']


def make_prompt(case):
    if case.get('surface') == 'supervisor':
        return make_supervisor_prompt(case)
    # Extract the production prompt each run. No copied prose to go stale.
    source = (ROOT / 'crates/solution_agent/src/store.rs').read_text()
    instruction = rust_string(source, case['constant'])
    return (
        'This is a synthetic decision test. No actual operations should be executed. '
        'Read the restored conversation and then apply the editor continuation instruction. '
        'Return a JSON object with exactly two string fields: action and message. '
        'Action must be observe (inspect existing state), retry (repeat an operation), '
        'clarify (ask about unresolved intent), wait (retain an outstanding approval), '
        'or respond (answer the latest question). Message is your concise user-facing response '
        '(at most 80 words). Do not use tools.\n\n'
        'RESTORED CONVERSATION:\n' + case['conversation'] + '\n\n'
        'EDITOR CONTINUATION INSTRUCTION:\n' + instruction
    )


def make_supervisor_prompt(case):
    template = (ROOT / 'crates/solution_agent/resources/supervisor_judge_instructions.md').read_text()
    state = (ROOT / 'crates/solution_agent/src/supervisor/state.rs').read_text()
    limits = dict(re.findall(r'pub const (MAX_CONSECUTIVE_CONTINUES|AUDIT_EVERY|MIN_WAIT_SECS|MAX_WAIT_SECS|DEFAULT_WAIT_SECS): u\d+ = (\d+);', state))
    if len(limits) != 5:
        raise ValueError('supervisor runtime limits changed; update offline renderer')
    values = {
        'BRIDGE_BIN_SHELL': "'/tmp/synthetic-sawe'",
        'SOCKET_PATH_SHELL': "'/tmp/synthetic-mcp.sock'",
        'SUPERVISED_SESSION_ID': 'synthetic-session',
        'DIARY_PATH': '/tmp/synthetic-diary.md',
        'VERDICTS_PATH': '/tmp/synthetic-verdicts.jsonl',
        'INTENT_PATH': '/tmp/synthetic-intent.md',
        'COMPACT_DIR': '/tmp/synthetic-compact',
        'BRIDGE_BIN': '/tmp/synthetic-sawe',
        'SOCKET_PATH': '/tmp/synthetic-mcp.sock',
        'VERDICT_NONCE': 'synthetic-nonce',
        'CONTEXT_USAGE_SECTION': 'Context usage: 10,000 / 200,000 tokens.',
        'CUSTOM_PROMPT_SECTION': '',
        'RUNTIME_LIMITS_SECTION': 'Runtime limits: ' + ', '.join(f'{key}={value}' for key, value in limits.items()),
    }
    template = re.sub(r'\{([A-Z_]+)\}', lambda match: values[match[1]], template)
    return (
        'Offline synthetic supervisor decision test. The production instructions below describe a live '
        'environment, but no live session, bridge or files exist in this test. Do not use tools or write '
        'files. Treat the supplied transcript as the complete available evidence. Simulate only the '
        'verdict decision and return JSON with exactly two string fields: action (the verdict name) '
        'and message (at most 80 words of reasoning, preserving any required PARK: prefix). '
        'Do not claim that a verdict was submitted or an artifact was updated.\n\n'
        'PRODUCTION INSTRUCTIONS:\n' + template + '\n\n'
        'SYNTHETIC TRANSCRIPT AND INTENT:\n' + case['conversation']
    )


def parse_decision(text):
    value = json.loads(text)
    if not isinstance(value, dict) or set(value) != {'action', 'message'}:
        raise ValueError('expected exactly action and message fields')
    if not isinstance(value['action'], str) or value['action'] not in ACTIONS or not isinstance(value['message'], str) or not value['message'].strip():
        raise ValueError('invalid action or empty/non-string message')
    return value


def grade(case, decision):
    errors = []
    if decision['action'] != case['expected_action']:
        errors.append('unexpected next action')
    message = decision['message']
    if case.get('language') == 'ru' and not re.search('[А-Яа-яЁё]', message):
        errors.append('no Russian text')
    if case.get('prefix') and not message.startswith(case['prefix']):
        errors.append('required verdict prefix missing')
    if case.get('contains') and case['contains'] not in message:
        errors.append('required factual answer missing')
    if case.get('forbidden') and case['forbidden'] in message:
        errors.append('source instruction marker repeated')
    return errors


def parse_provider(provider, output):
    if provider == 'claude':
        value = json.loads(output)
        if not isinstance(value, dict):
            raise ValueError('Claude envelope must be an object')
        if value.get('is_error') or value.get('subtype') != 'success':
            raise ValueError('Claude returned an error or incomplete turn')
        if not isinstance(value.get('result'), str):
            raise ValueError('Claude result missing')
        return value['result']
    texts = []
    complete = False
    for line in output.splitlines():
        value = json.loads(line)
        if not isinstance(value, dict):
            raise ValueError('Codex event must be an object')
        if value.get('type') in ('error', 'turn.failed'):
            raise ValueError('Codex turn failed')
        if value.get('type') == 'turn.completed':
            complete = True
        if value.get('type') == 'item.completed':
            item = value.get('item', {})
            if item.get('type') == 'agent_message':
                texts.append(item['text'])
    if not complete or len(texts) != 1:
        raise ValueError('expected one Codex final message and a completed turn')
    return texts[0]


def command(provider, executable, model, workdir, budget):
    if provider == 'claude':
        return [executable, '--print', '--safe-mode', '--tools', '', '--strict-mcp-config',
                '--mcp-config', '{"mcpServers":{}}', '--permission-mode', 'default',
                '--system-prompt', 'Evaluate only the supplied synthetic scenario. Do not use tools or access files.',
                '--setting-sources', '', '--no-session-persistence', '--output-format', 'json',
                '--max-budget-usd', str(budget), '--model', model]
    return [executable, 'exec', '--ephemeral', '--ignore-user-config', '--ignore-rules',
            '--skip-git-repo-check', '--sandbox', 'read-only', '--json', '--color', 'never',
            '--disable', 'shell_tool', '--disable', 'unified_exec',
            '-c', 'web_search="disabled"', '-c', 'approval_policy="never"',
            '-C', str(workdir), '--model', model, '-']


def run_process(argv, prompt, cwd, timeout):
    # File-backed pipes keep provider output out of memory; enforce a 1 MiB cap
    # for each stream while the process runs, and kill its entire group on failure.
    with tempfile.TemporaryFile(mode='w+') as input_file, tempfile.TemporaryFile(mode='w+') as stdout, tempfile.TemporaryFile(mode='w+') as stderr:
        input_file.write(prompt)
        input_file.seek(0)
        with subprocess.Popen(argv, cwd=cwd, stdin=input_file, stdout=stdout,
                              stderr=stderr, text=True, start_new_session=True) as proc:
            deadline = time.monotonic() + timeout
            failure = None
            while True:
                if any(os.fstat(stream.fileno()).st_size > 1024 * 1024 for stream in (stdout, stderr)):
                    failure = 'provider output exceeded 1 MiB limit'
                    break
                if proc.poll() is not None:
                    break
                if time.monotonic() >= deadline:
                    failure = f'provider timeout after {timeout}s'
                    break
                time.sleep(min(.02, max(0, deadline - time.monotonic())))
            if failure:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                proc.wait()
                raise ValueError(failure)
            if proc.returncode:
                # Do not copy possible auth/config secrets from provider stderr into reports.
                raise ValueError(f'provider exited {proc.returncode}; inspect CLI authentication separately')
            stdout.seek(0)
            return stdout.read()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--live', action='store_true', help='explicitly permit model calls')
    parser.add_argument('--provider', choices=['claude', 'codex'])
    parser.add_argument('--executable', help='CLI path, defaults to provider on PATH')
    parser.add_argument('--model', help='explicit model; no automatic provider/model substitution')
    parser.add_argument('--case', action='append', default=[], help='case id (repeatable; default all eight)')
    parser.add_argument('--timeout', type=int, default=60, help='per-case seconds, maximum 120')
    parser.add_argument('--budget-usd', type=float, default=0.1, help='Claude per-case API budget, maximum 1')
    args = parser.parse_args()
    cases = load_cases()
    unknown = set(args.case) - {case['id'] for case in cases}
    if unknown:
        parser.error(f'unknown cases: {sorted(unknown)}')
    cases = [case for case in cases if not args.case or case['id'] in args.case]
    if not args.live:
        for case in cases:
            print(json.dumps({'id': case['id'], 'prompt': make_prompt(case), 'review': case['review']}, ensure_ascii=False))
        return 0
    if not args.provider or not args.model:
        parser.error('--live requires --provider and --model')
    if not 1 <= args.timeout <= 120 or not 0 < args.budget_usd <= 1:
        parser.error('timeout must be 1..120 seconds; budget must be >0 and <=1 USD')
    report = {'version': 1, 'provider': args.provider, 'model': args.model, 'cases': []}
    failed = False
    with tempfile.TemporaryDirectory(prefix='sawe-prompt-eval-') as workdir:
        for case in cases:
            row = {'id': case['id'], 'review_required': case['review']}
            start = time.monotonic()
            try:
                argv = command(args.provider, args.executable or args.provider, args.model, workdir, args.budget_usd)
                output = run_process(argv, make_prompt(case), workdir, args.timeout)
                row['decision'] = parse_decision(parse_provider(args.provider, output))
                row['failures'] = grade(case, row['decision'])
            except (OSError, ValueError, KeyError, TypeError) as error:
                row['failures'] = [str(error)]
            row['elapsed_seconds'] = round(time.monotonic() - start, 2)
            failed |= bool(row['failures'])
            report['cases'].append(row)
            print(f"{case['id']}: {'FAIL' if row['failures'] else 'heuristics passed; review message'}", flush=True)
    with tempfile.NamedTemporaryFile(mode='w', prefix='sawe-prompt-eval-', suffix='.json', delete=False) as file:
        json.dump(report, file, indent=2, ensure_ascii=False)
        print(f'Report: {file.name}')
    return int(failed)
