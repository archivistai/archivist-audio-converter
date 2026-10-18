"""Diagnostic: how often does each cleanup-chain variant hang? Never fails the build."""
import pathlib
import subprocess
import sys
import tempfile
import time

ff = str(pathlib.Path(sys.argv[1]).resolve())
runs = int(sys.argv[2]) if len(sys.argv) > 2 else 6
timeout = 8
model_dir = pathlib.Path(__file__).resolve().parents[1] / 'src-tauri' / 'models'

HEAD = ['aformat=channel_layouts=mono', 'highpass=f=70:poles=2', 'aresample=48000']
DN = 'arnndn=m=sh.rnnn:mix=0.7'
SN = 'speechnorm=e=6:c=1:r=0.0001:l=1'
# Round 2: Windows hangs only when arnndn feeds speechnorm. Try audio-neutral separators.
VARIANTS = {
    'baseline (arnndn,speechnorm)': ([], HEAD + [DN, SN]),
    'asetnsamples between': ([], HEAD + [DN, 'asetnsamples=n=4096:p=0', SN]),
    'aresample between': ([], HEAD + [DN, 'aresample=48000', SN]),
    'aformat fltp between': ([], HEAD + [DN, 'aformat=sample_fmts=fltp', SN]),
    'swapped (speechnorm first)': ([], HEAD + [SN, DN]),
}


def render(pre, source, filters):
    start = time.time()
    try:
        r = subprocess.run([ff, '-hide_banner', '-nostdin', '-nostats', '-y', *pre, '-i', source,
                            '-af', ','.join(filters), '-f', 'null', '-'],
                           cwd=model_dir, capture_output=True, text=True, timeout=timeout)
        return 'ok' if r.returncode == 0 else 'error', time.time() - start
    except subprocess.TimeoutExpired:
        return 'hung', time.time() - start


def make(root, name, src_args):
    path = str(root / name)
    subprocess.run([ff, '-hide_banner', '-nostdin', '-y', '-f', 'lavfi', '-i', src_args,
                    '-c:a', 'aac', path], check=True, capture_output=True, timeout=60)
    return path


print(subprocess.run([ff, '-version'], capture_output=True, text=True).stdout.splitlines()[0])
with tempfile.TemporaryDirectory() as tmp:
    root = pathlib.Path(tmp)
    sources = {
        'sine': make(root, 'sine.aac', 'sine=d=10:r=48000'),
        'speech-like': make(root, 'speech.aac',
                            'anoisesrc=d=10:r=48000:c=pink:a=0.3,tremolo=f=4:d=0.9'),
    }
    print(f'{"variant":<28}{"source":<13}hung/{runs}  errors  max-ok-s', flush=True)
    for vname, (pre, filters) in VARIANTS.items():
        for sname, source in sources.items():
            res = [render(pre, source, filters) for _ in range(runs)]
            hung = sum(v == 'hung' for v, _ in res)
            errs = sum(v == 'error' for v, _ in res)
            slow = max([t for v, t in res if v == 'ok'] or [0])
            print(f'{vname:<28}{sname:<13}{hung:>6}  {errs:>6}  {slow:>8.1f}', flush=True)
