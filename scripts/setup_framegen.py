"""Build-time preparation of the complete, hash-pinned Windows RIFE payload.

Only CI/developers run Python. End users receive the DLLs and exported models.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import urllib.request
import uuid
import zipfile


SCRIPTS = Path(__file__).resolve().parent


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        while chunk := stream.read(4 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def verify(directory, manifest):
    expected_models = {f'{m}_{size}.onnx' for m in manifest['models'] for size in manifest['resolutions']}
    if {p.name for p in (directory / 'models').glob('*.onnx')} != expected_models:
        raise RuntimeError('Expected exactly the six supported RIFE model files')
    for entry in manifest['files']:
        path = directory / entry['path']
        if not path.is_file() or path.stat().st_size != entry['bytes'] or sha256(path) != entry['sha256']:
            raise RuntimeError('Frame-generation asset mismatch: ' + entry['path'])


def download(cache, name, entry):
    path = cache / name
    if path.is_file() and sha256(path) == entry['sha256']:
        return path
    temporary = cache / (name + '.partial-' + uuid.uuid4().hex)
    print('Downloading ' + name, flush=True)
    with urllib.request.urlopen(entry['url'], timeout=120) as source, temporary.open('xb') as target:
        shutil.copyfileobj(source, target)
    if sha256(temporary) != entry['sha256']:
        raise RuntimeError('Downloaded input checksum mismatch: ' + name)
    temporary.replace(path)
    return path


def prepare(output, cache, manifest):
    try:
        verify(output, manifest)
    except (RuntimeError, OSError):
        pass
    else:
        print('Complete pinned RIFE payload already verified: ' + str(output))
        return
    cache.mkdir(parents=True, exist_ok=True)
    output.parent.mkdir(parents=True, exist_ok=True)
    inputs = {name: download(cache, name, entry) for name, entry in manifest['build']['inputs'].items()}
    # TemporaryDirectory owns only this newly created, bounded staging directory.
    with tempfile.TemporaryDirectory(prefix='rife-stage-', dir=output.parent) as temporary:
        stage = Path(temporary).resolve()
        assert stage.parent == output.parent.resolve() and stage != output.parent.resolve()
        licenses = stage / 'licenses'
        licenses.mkdir()
        for name, entries in manifest['build']['archives'].items():
            with zipfile.ZipFile(inputs[name]) as archive:
                for source, relative in entries.items():
                    target = stage / relative
                    assert target.resolve().is_relative_to(stage)
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(archive.read(source))
        shutil.copyfile(inputs['Practical-RIFE-MIT.txt'], licenses / 'Practical-RIFE-MIT.txt')
        # Make notice bytes independent of checkout autocrlf settings.
        network_license = (SCRIPTS / 'rife/rifenet/LICENSE').read_bytes()
        (licenses / 'RIFE-network-MIT.txt').write_bytes(network_license.replace(b'\r\n', b'\n').replace(b'\n', b'\r\n'))
        weights = cache / 'weights'
        weights.mkdir(exist_ok=True)
        for name in ['flownet_v4.26.pkl', 'flownet_v4.6.pkl']:
            shutil.copyfile(inputs[name], weights / name)
        subprocess.run([sys.executable, str(SCRIPTS / 'rife/export_models.py'),
                        '--weights', str(weights), '--out', str(stage / 'models'),
                        '--models', *manifest['models']], check=True)
        import onnx
        for model in (stage / 'models').glob('*.onnx'):
            onnx.checker.check_model(str(model))
        verify(stage, manifest)
        # Replace only allowlisted build outputs after full staging verification.
        for entry in manifest['files']:
            target = output / entry['path']
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(stage / entry['path'], target)
    verify(output, manifest)
    print('Prepared and verified complete RIFE payload: ' + str(output))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=SCRIPTS.parent / '.deps/rife')
    parser.add_argument('--cache', type=Path, default=SCRIPTS.parent / '.deps/framegen-inputs')
    parser.add_argument('--verify-only', action='store_true')
    args = parser.parse_args()
    manifest = json.loads((SCRIPTS / 'framegen-assets.json').read_text(encoding='utf-8'))
    if args.verify_only:
        verify(args.output.resolve(), manifest)
    else:
        prepare(args.output.resolve(), args.cache.resolve(), manifest)


if __name__ == '__main__':
    main()
