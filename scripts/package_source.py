"""Create a source-only archive from a clean, reviewed Git checkout."""
from pathlib import Path
import hashlib
import subprocess
import sys
import zipfile
from check_release import ROOT, tracked, check

if subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=normal'], cwd=ROOT).strip():
    sys.exit('Commit/review changes and resolve untracked files before packaging.')
files = tracked()
errors = check(files)
if errors:
    sys.exit('\n'.join(errors))
revision = subprocess.check_output(['git', 'rev-parse', '--short=12', 'HEAD'], cwd=ROOT).decode().strip()
output = ROOT / 'build' / f'Shishi-Source-{revision}.zip'
output.parent.mkdir(exist_ok=True)
with zipfile.ZipFile(output, 'w', zipfile.ZIP_DEFLATED) as archive:
    for relative in files:
        archive.write(ROOT / relative, 'shishi/' + relative.as_posix())
print(output.name)
print('SHA256', hashlib.sha256(output.read_bytes()).hexdigest())
