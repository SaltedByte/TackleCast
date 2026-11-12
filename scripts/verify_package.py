"""Check packaged runtime libraries, rather than relying on developer PATHs.

--exe checks the standalone Windows executable: every DLL it imports must ship
with Windows, so the single file runs on its own.
--package checks the Linux package directory and its bundled lib/.
"""
import argparse
import json
import os
from pathlib import Path
import struct
import subprocess


def pe_imports(path):
    """DLL names in a PE file's import and delay-import tables."""
    data = Path(path).read_bytes()
    pe = struct.unpack_from('<I', data, 0x3C)[0]
    if data[pe:pe + 4] != b'PE\0\0':
        raise RuntimeError(f'{path} is not a PE executable')
    sections_count = struct.unpack_from('<H', data, pe + 6)[0]
    optional_size = struct.unpack_from('<H', data, pe + 20)[0]
    optional = pe + 24
    pe32_plus = struct.unpack_from('<H', data, optional)[0] == 0x20B
    directories = optional + (112 if pe32_plus else 96)
    sections = []
    for index in range(sections_count):
        virtual_size, virtual_address, raw_size, raw_offset = struct.unpack_from(
            '<IIII', data, optional + optional_size + index * 40 + 8)
        sections.append((virtual_address, max(virtual_size, raw_size), raw_offset))

    def offset(rva):
        for virtual_address, size, raw_offset in sections:
            if virtual_address <= rva < virtual_address + size:
                return rva - virtual_address + raw_offset
        raise RuntimeError(f'RVA {rva:#x} is outside every section')

    def name_at(rva):
        start = offset(rva)
        return data[start:data.index(b'\0', start)].decode('ascii')

    names = []
    # (data directory index, descriptor size, offset of the name RVA)
    for directory, size, name_field in ((1, 20, 12), (13, 32, 4)):
        rva = struct.unpack_from('<I', data, directories + directory * 8)[0]
        if not rva:
            continue
        descriptor = offset(rva)
        while any(data[descriptor:descriptor + size]):
            names.append(name_at(struct.unpack_from('<I', data, descriptor + name_field)[0]))
            descriptor += size
    return names


def verify_standalone_exe(exe):
    exe = exe.resolve()
    system32 = Path(os.environ.get('SystemRoot', r'C:\Windows')) / 'System32'
    system_dlls = {p.name.lower() for p in system32.glob('*.dll')}
    imports = sorted(set(pe_imports(exe)), key=str.lower)
    missing = [name for name in imports
               if name.lower() not in system_dlls
               # API sets resolve inside the loader rather than to a file.
               and not name.lower().startswith(('api-ms-win-', 'ext-ms-'))]
    if missing:
        raise RuntimeError('Standalone EXE needs DLLs that Windows does not provide: '
                           + ', '.join(missing))
    print(f'{exe.name}: {exe.stat().st_size / 2**20:.1f} MB, imports only Windows DLLs:')
    print('  ' + ', '.join(imports))


def verify_linux_package(package, commit=None):
    package = package.resolve()
    info = json.loads((package / 'build-info.json').read_text(encoding='utf-8-sig'))
    if commit and info['commit'] != commit:
        raise RuntimeError('Packaged source commit does not match the requested release')
    components = ['avcodec', 'avformat', 'avdevice', 'avutil', 'avfilter', 'swresample', 'swscale']
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
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument('--exe', type=Path, help='standalone Windows executable')
    target.add_argument('--package', type=Path, help='Linux package directory')
    parser.add_argument('--commit')
    args = parser.parse_args()
    if args.exe:
        verify_standalone_exe(args.exe)
    else:
        verify_linux_package(args.package, args.commit)
