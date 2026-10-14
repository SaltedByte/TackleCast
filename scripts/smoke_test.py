"""Render a synthetic pattern from a packaged executable, without capture hardware."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time


def smoke(binary, seconds):
    binary = binary.resolve()
    package = binary.parent
    env = os.environ.copy()
    if os.name == 'nt':
        env['PATH'] = os.path.join(os.environ['SystemRoot'], 'System32')
    else:
        env['LD_LIBRARY_PATH'] = str(package / 'lib')
    with tempfile.TemporaryDirectory(prefix='tacklecast-smoke-') as workdir:
        started = time.time()
        with open(Path(workdir) / 'stdout.log', 'w') as output:
            process = subprocess.Popen([str(binary), '--test-nv12'], cwd=workdir, env=env,
                                       stdout=output, stderr=subprocess.STDOUT)
            try:
                process.wait(timeout=seconds)
                raise RuntimeError(f'Synthetic renderer exited early: {process.returncode}')
            except subprocess.TimeoutExpired:
                pass
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=10)
        candidates = list((package / 'logs').glob('*.log')) + list((Path(workdir) / 'logs').glob('*.log'))
        logs = [p for p in candidates if p.stat().st_mtime >= started - 1]
        if not logs:
            raise RuntimeError('No runtime log produced')
        text = max(logs, key=lambda p: p.stat().st_mtime).read_text()
        if any(error in text for error in ['panic:', 'renderer initialization failed', 'render error:']):
            raise RuntimeError(text)
        rates = re.findall(r'render summary: ([\d.]+) rendered fps, ([\d.]+) uploaded fps', text)
        if not rates or not any(float(rendered) > 0 and float(uploaded) > 0 for rendered, uploaded in rates):
            raise RuntimeError('No successful render summary: ' + text)
        print(f'Packaged synthetic rendering passed for {seconds}s: {rates[-1]} rendered/uploaded FPS')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--seconds', type=int, default=35)
    args = parser.parse_args()
    smoke(args.binary, args.seconds)
