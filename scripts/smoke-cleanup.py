"""Exercise the cleanup, measurement and encode passes with the bundled FFmpeg binaries.

Two halves, and both are load-bearing:

* `hang_check` renders a LONG speech-like clip through the real cleanup chain. FFmpeg 9.x
  livelocks in `arnndn` -> `speechnorm`, but only on long, varying input - the short sine
  below completes happily on 9.0.1, the exact build the pin exists to avoid. Without this,
  a pin bumped to a hanging ffmpeg sails through CI and freezes users mid-conversion.
* The option matrix then runs every cleanup/loudness combination on a short clip, so a
  filter rename or a behaviour change still fails fast.
"""
import json
import pathlib
import re
import subprocess
import sys
import tempfile
import time

ff, probe = map(lambda p: str(pathlib.Path(p).resolve()), sys.argv[1:3])
model_dir = pathlib.Path(__file__).resolve().parents[1] / 'src-tauri' / 'models'

# Measured on macOS arm64 against the 9.0.1 sidecar: 120s of speech-like audio livelocks
# 8/8 runs, while 10s of the same signal completes in 0.1s. Duration is the trigger.
HANG_SECONDS = 120
# A healthy arnndn pass runs ~100x realtime on Apple Silicon and well above realtime even
# on the slowest hosted runner, so allowing a full 1x realtime leaves enormous headroom
# while still catching a livelock, which never finishes at all.
HANG_TIMEOUT = HANG_SECONDS
# README: Windows stalled in roughly 3 of 4 runs. One clean pass is not evidence.
HANG_REPEATS = 3


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


def hang_check(root):
    """Fail loudly if the cleanup chain wedges on a long recording."""
    speech = str(root / 'long-speech.aac')
    # Pink noise with a syllable-rate tremolo has the pauses and level swings of speech;
    # a steady tone does not, and does not trip the bug.
    run('-f', 'lavfi', '-i',
        f'anoisesrc=d={HANG_SECONDS}:r=48000:c=pink:a=0.3,tremolo=f=4:d=0.9',
        '-c:a', 'aac', speech, timeout=120)

    # Mirrors `CleanupOptions::transcription()` in chain.rs (highpass 70, neural denoise
    # at mix 0.85, speechnorm e=6 c=1, 16 kHz mono) - the preset the Discord report came
    # from. Keep in sync with that preset; a near-miss may not trip the bug.
    filters = ['aformat=channel_layouts=mono', 'highpass=f=70:poles=2', 'aresample=48000',
               'arnndn=m=sh.rnnn:mix=0.85', 'speechnorm=e=6:c=1:r=0.0001:l=1',
               'aresample=16000']

    for attempt in range(1, HANG_REPEATS + 1):
        out = str(root / f'long-clean-{attempt}.flac')
        start = time.time()
        try:
            run('-i', speech, '-map', '0:a:0', '-vn', '-af', ','.join(filters),
                '-c:a', 'flac', '-sample_fmt', 's32', out, timeout=HANG_TIMEOUT)
        except TimeoutError as e:
            raise SystemExit(
                f'FAIL: the cleanup chain hung on {HANG_SECONDS}s of speech-like audio '
                f'(run {attempt}/{HANG_REPEATS}, killed after {HANG_TIMEOUT}s).\n'
                'This is the arnndn -> speechnorm livelock. Do NOT ship this ffmpeg: it '
                'freezes mid-conversion with no error and no exit.\n'
                f'{e}') from None
        elapsed = time.time() - start
        # A chain that quietly truncates is as bad as one that hangs, and both leave an
        # exit code of 0 behind.
        info = json.loads(subprocess.check_output(
            [probe, '-v', 'error', '-show_format', '-of', 'json', out]))
        assert abs(float(info['format']['duration']) - HANG_SECONDS) < 1.0, info
        print(f'PASS hang check {attempt}/{HANG_REPEATS} '
              f'({elapsed:.1f}s for {HANG_SECONDS}s of audio)', flush=True)


with tempfile.TemporaryDirectory() as tmp:
    root = pathlib.Path(tmp)
    source = str(root / 'source.aac')
    run('-f', 'lavfi', '-i', 'sine=d=10:r=48000', '-c:a', 'aac', source)
    # First, because it is the check that would actually have caught the 9.x regression.
    hang_check(root)
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
