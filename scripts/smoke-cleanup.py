"""Exercise the sequential cleanup boundary with actual bundled FFmpeg binaries."""
import json
import pathlib
import re
import subprocess
import sys
import tempfile
import time

ff, probe = map(lambda p: str(pathlib.Path(p).resolve()), sys.argv[1:3])
model_dir = pathlib.Path(__file__).resolve().parents[1] / 'src-tauri' / 'models'


def run(*args, timeout=60):
    try:
        result = subprocess.run([ff, '-hide_banner', '-nostdin', '-nostats', '-y', *args],
                                cwd=model_dir, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired as e:
        tail = e.stderr.decode(errors='replace') if isinstance(e.stderr, bytes) else (e.stderr or '')
        raise TimeoutError(f'ffmpeg hung for {timeout}s: {args}\n{tail[-2000:]}') from None
    if result.returncode:
        raise RuntimeError(result.stderr)
    return result.stderr


def try_render(label, source, filters, *out):
    start = time.time()
    try:
        run('-i', source, '-map', '0:a:0', '-vn', '-af', ','.join(filters), *out, timeout=20)
        verdict = 'ok'
    except TimeoutError:
        verdict = 'HUNG'
    except RuntimeError:
        verdict = 'ERROR'
    print(f'  diagnose: {label:<46} {verdict} {time.time() - start:.1f}s', flush=True)


def diagnose(root, source, filters):
    """Name what hangs: which filter, which input signal, which output writer."""
    speech = str(root / 'speechlike.aac')
    # Pink noise with a syllable-rate tremolo has pauses and level swings, unlike a steady tone.
    run('-f', 'lavfi', '-i', 'anoisesrc=d=10:r=48000:c=pink:a=0.3,tremolo=f=4:d=0.9',
        '-c:a', 'aac', speech)
    for i in range(1, len(filters) + 1):
        try_render(f'sine, {filters[i - 1].split("=")[0]} (chain of {i}) -> null',
              source, filters[:i], '-f', 'null', '-')
    writers = {'flac s32': ('flac', ['-c:a', 'flac', '-sample_fmt', 's32']),
               'flac s16': ('flac', ['-c:a', 'flac']),
               'wav 24-bit': ('wav', ['-c:a', 'pcm_s24le']),
               'null': (None, ['-f', 'null'])}
    for name, src in (('sine', source), ('speech-like', speech)):
        for wname, (ext, wargs) in writers.items():
            target = str(root / f'probe-{wname[:3]}.{ext}') if ext else '-'
            try_render(f'{name}, full chain -> {wname}', src, filters, *wargs, target)


with tempfile.TemporaryDirectory() as tmp:
    root = pathlib.Path(tmp)
    source = str(root / 'source.aac')
    run('-f', 'lavfi', '-i', 'sine=d=10:r=48000', '-c:a', 'aac', source)
    for compressor in (False, True):
        for ebu in (False, True):
            stage = str(root / 'clean.flac')
            output = str(root / 'output.mp3')
            filters = ['aformat=channel_layouts=mono', 'highpass=f=70:poles=2',
                       'aresample=48000', 'arnndn=m=sh.rnnn:mix=0.7',
                       'speechnorm=e=6:c=1:r=0.0001:l=1']
            if compressor:
                filters += ['acompressor=threshold=-18dB:ratio=3:attack=20:release=250:makeup=2']
            try:
                run('-i', source, '-map', '0:a:0', '-vn', '-af', ','.join(filters),
                    '-c:a', 'flac', '-sample_fmt', 's32', stage)
            except TimeoutError as e:
                print(e, flush=True)
                diagnose(root, source, filters)
                raise
            log = run('-i', stage, '-af', 'loudnorm=I=-15:TP=-1.5:LRA=20:print_format=json',
                      '-f', 'null', '-')
            stats = json.loads(re.search(r'\{[^{}]*"input_i"[^{}]*\}', log).group())
            if ebu:
                correction = ('loudnorm=I=-15:TP=-1.5:LRA=20:linear=true:' + ':'.join(
                    f'{key}={stats[value]}' for key, value in
                    [('measured_I', 'input_i'), ('measured_TP', 'input_tp'),
                     ('measured_LRA', 'input_lra'), ('measured_thresh', 'input_thresh')]))
            else:
                gain = max(-30, min(30, -15-float(stats['input_i']), -1.5-float(stats['input_tp'])))
                correction = f'volume={gain}dB,alimiter=limit=0.841395:level=false'
            run('-i', stage, '-af', correction, '-c:a', 'libmp3lame', '-q:a', '2', output)
            info = json.loads(subprocess.check_output(
                [probe, '-v', 'error', '-show_format', '-show_streams', '-of', 'json', output]))
            assert abs(float(info['format']['duration']) - 10) < 0.3, info
            assert info['streams'][0]['channels'] == 1, info
            run('-i', output, '-f', 'null', '-')  # fully decode, not just inspect headers
            print(f'PASS compression={compressor}, R128={ebu}', flush=True)
