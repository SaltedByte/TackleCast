"""Check packaged runtime libraries, rather than relying on developer PATHs."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import subprocess


def verify(package, require_gpu=False, commit=None, require_framegen=False):
    package = package.resolve()
    info = json.loads((package / 'build-info.json').read_text(encoding='utf-8-sig'))
    if commit and info['commit'] != commit:
        raise RuntimeError('Packaged source commit does not match the requested release')
    if require_framegen and not info.get('framegen_bundled'):
        raise RuntimeError('The complete frame-generation payload is required')
    components = ['avcodec', 'avformat', 'avdevice', 'avutil', 'avfilter', 'swresample', 'swscale']
    if os.name == 'nt':
        with os.add_dll_directory(str(package)):
            for component in components:
                libraries = list(package.glob(component + '-*.dll'))
                if len(libraries) != 1:
                    raise RuntimeError(f'Expected one {component} runtime DLL, found {libraries}')
                ctypes.WinDLL(str(libraries[0]), winmode=0x1100)
            if require_gpu:
                pairs = [(f'nvjpeg64_{major}.dll', f'cudart64_{major}.dll' if major != 11 else 'cudart64_110.dll') for major in (13, 12, 11)]
                found = [pair for pair in pairs if all((package / name).is_file() for name in pair)]
                if len(found) != 1:
                    raise RuntimeError('Expected exactly one complete supported nvJPEG/CUDA runtime pair')
                for name in found[0]:
                    ctypes.WinDLL(str(package / name), winmode=0x1100)
            if info.get('vc_runtime_bundled'):
                for name in ['msvcp140.dll', 'msvcp140_1.dll', 'vcruntime140.dll', 'vcruntime140_1.dll']:
                    if not (package / name).is_file():
                        raise RuntimeError('Missing app-local Microsoft runtime: ' + name)
                for entry in info['vc_runtime_bundled']:
                    path = package / entry['file']
                    if hashlib.sha256(path.read_bytes()).hexdigest() != entry['sha256']:
                        raise RuntimeError('Microsoft runtime hash mismatch: ' + entry['file'])
            if info.get('framegen_bundled'):
                pins = json.loads((package / 'licenses/framegen-assets.json').read_text(encoding='utf-8'))
                expected_models = {f'{model}_{size}.onnx' for model in pins['models'] for size in pins['resolutions']}
                actual_models = {p.name for p in (package / 'rife/models').glob('*.onnx')}
                if actual_models != expected_models or len(expected_models) != 6:
                    raise RuntimeError('Expected exactly the six bundled RIFE 4.26/4.6 models')
                for entry in pins['files']:
                    relative = Path(entry['path'])
                    path = package / relative if relative.parts[0] == 'licenses' else package / 'rife' / relative
                    if path.stat().st_size != entry['bytes'] or hashlib.sha256(path.read_bytes()).hexdigest() != entry['sha256']:
                        raise RuntimeError('Frame-generation asset mismatch: ' + entry['path'])
                for name in ['DirectML.dll', 'onnxruntime_providers_shared.dll', 'onnxruntime.dll']:
                    ctypes.WinDLL(str(package / 'rife' / name), winmode=0x1100)
    else:
        library_dir = package / 'lib'
        env = os.environ.copy()
        env['LD_LIBRARY_PATH'] = str(library_dir)
        dependencies = subprocess.check_output(['ldd', str(package / 'tacklecast')], env=env, text=True)
        if 'not found' in dependencies:
            raise RuntimeError(dependencies)
        for component in components:
            library = library_dir / ('lib' + component + '.so')
            if not library.exists() or not library.resolve().is_relative_to(library_dir):
                raise RuntimeError(f'{component} is missing from bundled lib/')
            # --as-needed can omit unused FFmpeg components from the executable;
            # check their dependencies too, rather than requiring a NEEDED entry.
            dependencies += subprocess.check_output(['ldd', str(library)], env=env, text=True)
        if 'not found' in dependencies:
            raise RuntimeError(dependencies)
        for line in dependencies.splitlines():
            if any(line.strip().startswith('lib' + name + '.so') for name in components):
                if str(library_dir) not in line:
                    raise RuntimeError('FFmpeg resolved outside bundled lib/: ' + line)
        print(dependencies)
    print('Package verified:', info)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--package', type=Path, required=True)
    parser.add_argument('--require-gpu', action='store_true')
    parser.add_argument('--commit')
    parser.add_argument('--require-framegen', action='store_true')
    args = parser.parse_args()
    verify(args.package, args.require_gpu, args.commit, args.require_framegen)
