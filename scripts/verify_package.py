"""Check packaged runtime libraries, rather than relying on developer PATHs."""
import argparse
import ctypes
import json
import os
from pathlib import Path
import subprocess


def verify(package, require_gpu=False, commit=None):
    package = package.resolve()
    info = json.loads((package / 'build-info.json').read_text(encoding='utf-8-sig'))
    if commit and info['commit'] != commit:
        raise RuntimeError('Packaged source commit does not match the requested release')
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
    args = parser.parse_args()
    verify(args.package, args.require_gpu, args.commit)
