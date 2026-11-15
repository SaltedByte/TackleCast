"""Exercise opt-in and diagnostic summaries through an extracted Linux launcher.

Synthetic ring/resampler evidence only: no CPAL, Bazzite, capture card, speakers,
or display timing validation. The package folder should be a disposable extract.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time


def verify(package, render=False):
    package = package.resolve()
    launcher = package / 'run.sh'
    env = os.environ.copy()
    env.pop('LD_LIBRARY_PATH', None)
    env.pop('TACKLECAST_DIAGNOSTICS', None)
    # Deliberately hostile RUST_LOG tests that diagnostic privacy filtering wins.
    env['RUST_LOG'] = 'trace'
    with tempfile.TemporaryDirectory() as cwd:
        subprocess.run([str(launcher), '--diagnostic-audio-fixture'], cwd=cwd,
                       env=env, check=True, capture_output=True, timeout=15)
        off = max((package / 'logs').glob('tacklecast_*.log'), key=lambda p: p.stat().st_mtime)
        assert 'drift startup' not in off.read_text(), 'Diagnostics unexpectedly enabled'
        time.sleep(1.1)  # legacy log filename resolution is one second
        env['TACKLECAST_DIAGNOSTICS'] = '1'
        subprocess.run([str(launcher), '--diagnostic-audio-fixture'], cwd=cwd,
                       env=env, check=True, capture_output=True, timeout=60)
    log = max((package / 'logs').glob('tacklecast_*.log'), key=lambda p: p.stat().st_mtime)
    text = log.read_text()
    info = json.loads((package / 'build-info.json').read_text())
    assert f'build={info["commit"]} version={info["version"]}' in text
    assert '-dirty' not in text, 'Diagnostic executable must identify a clean source commit'
    assert 'fixture complete' in text
    assert all('drift ' in line for line in text.splitlines()), 'Legacy logs escaped privacy filter'
    assert not any(s in text for s in ['/work', '/home/', '/root/', '/dev/v4l/by-id/', 'loaded settings'])
    scenarios = dict(re.findall(r'fixture scenario=(\w+) id=(\d+)', text))
    finals = {}
    for line in text.splitlines():
        match = re.search(r'id=(\d+) kind=final .*?total=(\[[^]]+\])', line)
        if match:
            finals[match[1]] = json.loads(match[2])
    steady = finals[scenarios['steady']]
    mismatch = finals[scenarios['rate_mismatch']]
    underrun = finals[scenarios['forced_underrun']]
    resampled = finals[scenarios['resampled']]
    for values in finals.values():
        assert values[1] == values[2] + values[6], values
        assert values[3] == values[4] + values[5], values
    assert steady[5] == steady[6] == mismatch[5] == mismatch[6] == 0
    assert mismatch[2] - mismatch[4] > steady[2] - steady[4]
    assert underrun[5] > 0 and underrun[6] == 0
    # Seed frames are offered at the ring, not through capture/resampling.
    assert abs((resampled[1] - 9600) / 48000 - resampled[0] / 44100) < 0.0001
    for stream in scenarios.values():
        assert re.search(rf'id={stream} kind=interval ', text), 'Missing periodic summary'
    print('Diagnostic launcher opt-in OFF/ON, privacy, identity, summaries and scenarios passed')
    print('Synthetic evidence:', log)
    if render:
        with tempfile.TemporaryDirectory() as cwd:
            process = subprocess.Popen([str(launcher), '--test-nv12'], cwd=cwd,
                                       env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                time.sleep(6)
                assert process.poll() is None, 'Diagnostic renderer exited early'
            finally:
                if process.poll() is None:
                    process.terminate()
                process.wait(timeout=10)
        log = max((package / 'logs').glob('tacklecast_*.log'), key=lambda p: p.stat().st_mtime)
        text = log.read_text()
        assert all('drift ' in line for line in text.splitlines())
        for stage in ['decoded_publish', 'ui_upload', 'present_return']:
            assert any(int(count) > 0 for count in re.findall(rf'{stage}=\((\d+),', text)), stage
        print('Diagnostic synthetic renderer and host video markers passed:', log)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--package', type=Path, required=True)
    parser.add_argument('--render', action='store_true', help='Also exercise video markers; requires desktop or Xvfb')
    args = parser.parse_args()
    verify(args.package, args.render)
