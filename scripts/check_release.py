"""Read-only checks of Git-tracked release inputs. Does not print secret values."""
from pathlib import Path
import os
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
TOP_FILES = {'.gitignore', 'AGENTS.md', 'Cargo.toml', 'Cargo.lock', 'build.rs', 'build.ps1',
             'README.md', 'README.txt', 'PLUGIN_API.txt', 'LICENSE', 'SECURITY.md',
             'CONTRIBUTING.md', 'THIRD_PARTY_NOTICES.md', 'AI_BACKENDS.md'}
TOP_DIRS = {'src', 'assets', 'config', 'docs', 'scripts', 'tests', 'plugin-sdk', 'examples', 'vendor', 'providers', 'ui'}
ALLOWED_EXT = {'.rs', '.toml', '.lock', '.md', '.txt', '.json', '.js', '.cjs', '.html', '.ps1',
               '.py', '.css', '.nsi', '.rc', '.png', '.ico', '.rgba', '.zip', '.spdx', '.orig', '.kt',
               '.pro', '.wasm', '.yml', '.yaml'}
TEXT_EXT = {'.rs', '.toml', '.lock', '.md', '.txt', '.json', '.js', '.cjs', '.html', '.ps1',
            '.py', '.css', '.nsi', '.rc', '.spdx', '.orig', '.kt', '.pro', '.yml', '.yaml'}
TOP_DIRS.add('installer')
SECRET = re.compile(r'-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----|(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9_-]{24,})')
PRIVATE_IP = re.compile(r'\b(?:10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3})\b')

def load_private_terms():
    """Optional private vocabulary stays outside the source repository."""
    configured = os.environ.get('SHISHI_PRIVATE_TERMS_FILE', '').strip()
    if not configured:
        return None
    try:
        path = Path(configured).expanduser().resolve()
    except (OSError, RuntimeError):
        raise RuntimeError('Private terms file could not be read.') from None
    if path.is_relative_to(ROOT.resolve()):
        raise RuntimeError('Private terms file must be outside the repository.')
    try:
        text = path.read_text(encoding='utf-8-sig')
    except (OSError, UnicodeError):
        raise RuntimeError('Private terms file could not be read.') from None
    terms = [line.strip() for line in text.splitlines()
             if line.strip() and not line.lstrip().startswith('#')]
    return re.compile('|'.join(re.escape(term) for term in terms), re.I) if terms else None

def tracked():
    """Tracked release inputs plus not-yet-committed files that would ship.
    New provider/source files must be scanned before `git add`, so a publication
    check cannot pass by ignoring the untracked worktree."""
    paths = []
    for args in (['git', 'ls-files', '-z'], ['git', 'ls-files', '-z', '--others', '--exclude-standard']):
        raw = subprocess.check_output(args, cwd=ROOT)
        paths.extend(p for p in raw.decode('utf-8').split('\0') if p)
    files = [Path(p) for p in dict.fromkeys(paths)]
    if not files:
        raise RuntimeError('No tracked files; stage the explicit source allowlist first.')
    return files

def allowed(path):
    parts = path.parts
    if len(parts) == 1:
        return path.name in TOP_FILES
    if parts[0] not in TOP_DIRS or any(p in {'.git', 'target', 'build', 'data', 'node_modules', '__pycache__'} for p in parts):
        return False
    if path.suffix == '.zip':
        return path.parent.as_posix() == 'tests/fixtures/plugins'
    return path.suffix in ALLOWED_EXT or path.name.startswith(('LICENSE', '.license')) or path.name in {'.gitignore', '.gitattributes'}

def check(files):
    private_terms = load_private_terms()
    errors = []
    for relative in files:
        path = ROOT / relative
        if not allowed(relative) or path.is_symlink() or not path.is_file():
            errors.append(f'{relative}: forbidden path or file type')
            continue
        if path.suffix not in TEXT_EXT and path.name not in TOP_FILES:
            continue
        text = path.read_text(encoding='utf-8-sig')
        # Vendored upstream examples may have author/contact metadata; retained
        # licenses are not rewritten. Secrets are checked everywhere.
        for number, line in enumerate(text.splitlines(), 1):
            if SECRET.search(line):
                errors.append(f'{relative}:{number}: possible credential')
            if relative.parts[0] != 'vendor' and (PRIVATE_IP.search(line) or
                    (private_terms is not None and private_terms.search(line))):
                errors.append(f'{relative}:{number}: private vocabulary or network address')
    return errors

if __name__ == '__main__':
    try:
        files = tracked()
        errors = check(files)
    except OSError:
        sys.exit('Release input or private terms file could not be read.')
    except (RuntimeError, subprocess.CalledProcessError, UnicodeError) as exc:
        sys.exit(str(exc))
    if errors:
        print('\n'.join(errors))
        sys.exit(1)
    print(f'PASS: {len(files)} tracked/untracked paths checked; no matching sensitive patterns. Manual review still required.')
