#!/usr/bin/env bash
# Build the standard Linux runtime, then make the issue25 bundle GUI-launchable.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"
command -v patchelf >/dev/null
bash scripts/package_linux_release.sh
python3 - <<'PY'
from pathlib import Path
import hashlib, json, os, shutil, subprocess, tempfile, zipfile

source = Path('dist/TackleCast-Linux-x86_64')
package = Path(tempfile.mkdtemp(prefix='issue25-package-', dir='dist')) / 'TackleCast-Issue25-Diagnostics'
shutil.copytree(source, package, symlinks=True)
# Native executable resolves its bundled libraries without a terminal/launcher.
subprocess.run(['patchelf', '--set-rpath', '$ORIGIN/lib', str(package / 'tacklecast')], check=True)
(package / 'diagnostics.enabled').write_text('Issue #25 diagnostic logging opt-in.\n')
(package / 'README.txt').write_text('''TackleCast - issue #25 diagnostic build

Extract the ZIP, open this folder, and double-click tacklecast.
If Linux asks, choose Run/Execute. If needed, open the file Properties >
Permissions and allow executing it as a program. No terminal is needed.
Keep all extracted files together. Diagnostic logging is already enabled.

The build reuses settings from your previous TackleCast folder in AppImages
or Downloads when available, leaving the original installation unchanged.
Keep your original generic pipewire input/output and other settings unchanged.

Play until audio drifts or for roughly as long as it usually takes to occur.
Close TackleCast normally and send the newest file in this folder's logs folder.
Include when you first noticed drift and whether audio was ahead or behind.
If it does not happen, tell us how long you played.

This is a diagnostics-only build based on v2.2.0, not a drift fix.
''')
info_path = package / 'build-info.json'
info = json.loads(info_path.read_text())
info.update(diagnostic_opt_in='diagnostics.enabled', direct_launch=True)
info_path.write_text(json.dumps(info, indent=2) + '\n')
archive = Path('dist/TackleCast-Linux-Issue25-Diagnostics.zip')
with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    for p in sorted(package.rglob('*')):
        if p.is_dir() or 'logs' in p.relative_to(package).parts or p.name == 'tacklecast_settings.json':
            continue
        entry = zipfile.ZipInfo(str(p.relative_to(package.parent)))
        entry.create_system = 3
        entry.external_attr = p.lstat().st_mode << 16
        entry.compress_type = zipfile.ZIP_DEFLATED
        z.writestr(entry, os.readlink(p).encode() if p.is_symlink() else p.read_bytes())
digest = hashlib.sha256(archive.read_bytes()).hexdigest()
archive.with_suffix('.zip.sha256').write_text(f'{digest}  {archive.name}\n')
print(f'GUI-launchable diagnostic ZIP: {archive}\nSHA256: {digest}')
PY
