"""Small structural checks, deliberately not a language or model-quality oracle."""
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
INVENTORY = ROOT / 'script/prompt_checks/inventory.json'
HBS = re.compile(r'\{\{\{.*?\}\}\}|\{\{.*?\}\}', re.S)
PLACEHOLDER = re.compile(r'\{\{[a-z_]+\}\}|\{[A-Z_]+\}')


def expressions(text):
    text = re.sub(r'\{\{!--.*?--\}\}', '', text, flags=re.S)
    text = re.sub(r'\{\{![^}]*\}\}', '', text)
    return sorted(set(' '.join(item.split()) for item in HBS.findall(text)))


def validate_template(text, entry, consumer=None):
    actual = expressions(text) if entry['kind'] == 'handlebars' else sorted(set(PLACEHOLDER.findall(text)))
    expected = entry['placeholders']
    errors = []
    uncommented = re.sub(r'\{\{!--.*?--\}\}', '', text, flags=re.S)
    remainder = HBS.sub('', uncommented) if entry['kind'] == 'handlebars' else PLACEHOLDER.sub('', text)
    malformed = ('{{' in remainder or '}}' in remainder) if entry['kind'] == 'handlebars' else re.search(r'\{\{[A-Za-z_]|\{[A-Z_]+', remainder)
    if malformed:
        errors.append('malformed or unknown template delimiter')
    if set(actual) != set(expected):
        errors.append(f"missing {sorted(set(expected)-set(actual))}; unknown {sorted(set(actual)-set(expected))}")
    if entry['kind'] == 'handlebars':
        stack = []
        for token in HBS.findall(re.sub(r'\{\{!--.*?--\}\}', '', text, flags=re.S)):
            body = token.strip('{}').strip()
            if body.startswith('#'):
                stack.append(body[1:].split()[0])
            elif body.startswith('/'):
                if not stack or stack.pop() != body[1:]:
                    errors.append(f'unbalanced block {token}')
            elif body == 'else' and not stack:
                errors.append('else outside block')
        if stack:
            errors.append(f'unclosed blocks: {stack}')
    elif consumer is not None:
        # Read actual renderer tuple keys, not comments mentioning placeholders.
        provided = set(re.findall(r'\(\s*"(\{\{[a-z_]+\}\}|\{[A-Z_]+\})"\s*,', consumer))
        if missing := set(actual) - provided:
            errors.append(f'no host replacement for {sorted(missing)}')
    return errors


def rust_string(source, name):
    match = re.search(r'\bconst\s+' + re.escape(name) + r'\s*:\s*&str\s*=\s*"((?:\\.|[^"\\])*)";', source, re.S)
    if not match:
        raise ValueError(f'cannot extract Rust string constant {name}')
    # Rust escaped-newline whitespace removal; JSON handles ordinary string escapes.
    raw = re.sub(r'\\\n\s*', '', match[1])
    return json.loads('"' + raw + '"')


def check(root=ROOT, inventory=None):
    inventory = inventory or json.loads(INVENTORY.read_text())
    if inventory.get('version') != 1:
        raise ValueError('unsupported inventory version')
    errors = []
    seen = set()
    for entry in inventory['surfaces']:
        path = entry['path']
        if path in seen:
            errors.append(f'duplicate inventory path: {path}')
        seen.add(path)
        try:
            text = (root / path).read_text()
            consumer = (root / entry['consumer']).read_text() if entry.get('consumer') else None
            if entry['kind'] in ('handlebars', 'substitution'):
                errors += [f'{path}: {error}' for error in validate_template(text, entry, consumer)]
            for name in entry.get('constants', []):
                rust_string(text, name)
        except (OSError, ValueError) as error:
            errors.append(f'{path}: {error}')
    for pattern in inventory['template_globs']:
        for path in root.glob(pattern):
            if path.as_posix().removeprefix(root.as_posix() + '/') not in seen:
                errors.append(f'uninventoried template: {path.relative_to(root)}')
    # The score examples run against the regex actually used by the Rust consumer.
    for contract in inventory['output_contracts']:
        source = (root / contract['consumer']).read_text()
        matches = re.findall(r'regex::Regex::new\(r"([^"]+)"\)', source)
        if contract['regex'] not in matches:
            errors.append(f"{contract['id']}: consumer regex changed; review fixtures")
            continue
        parser = re.compile(contract['regex'])
        for case in contract['cases']:
            match = parser.search(case['output'])
            value = int(match.group(1)) if match else None
            if value != case['parsed']:
                errors.append(f"{contract['id']}: output fixture mismatch {case}")
    return errors


def main():
    try:
        errors = check()
    except (OSError, ValueError, KeyError) as error:
        errors = [str(error)]
    if errors:
        print('\n'.join(errors))
        return 1
    print('Prompt inventory, template placeholders and output contract fixtures passed.')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
