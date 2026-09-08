# Audio Converter

A small Tauri desktop app: drag-and-drop (or browse for) audio files, clean
them up, and convert them to another format. It is aimed at people preparing
multi-speaker recordings — long tabletop-RPG sessions, interviews, podcasts —
for **speaker diarization and transcription**: it removes background noise,
lifts quiet speakers, and normalizes loudness. All processing is done by
bundled `ffmpeg`/`ffprobe` binaries ("sidecars") — no separate ffmpeg install
required on the end user's machine.

Stack: Tauri v2 (Rust backend) + plain HTML/CSS/JS frontend (no bundler
needed). GitHub Actions builds and releases installers for macOS (Intel +
Apple Silicon) and Windows when you push a version tag; macOS builds are
code-signed and notarized when the Apple Developer secrets below are set.

## Project layout

```
audio-converter/
├── src/                     # frontend (plain HTML/CSS/JS + archivist-logo.png)
├── src-tauri/
│   ├── src/
│   │   ├── main.rs          # Tauri commands + two-pass orchestration
│   │   ├── chain.rs         # filter-chain builder + presets (pure, unit-tested)
│   │   ├── ffmpeg.rs        # sidecar runner: progress, loudness JSON, ffprobe
│   │   ├── jobs.rs          # running-child registry, for cancel
│   │   └── paths.rs         # output naming, self-deleting temp files
│   ├── models/sh.rnnn       # bundled RNNoise model for the neural denoiser
│   ├── tauri.conf.json
│   ├── capabilities/        # Tauri v2 permission grants
│   └── icons/                # app icons (placeholders — see below)
├── icon-source.png           # 1024×1024 source art used to regenerate icons
└── .github/workflows/release.yml
```

## First-time local setup

1. Install [Rust](https://rustup.rs) and [Node.js 20+](https://nodejs.org).
2. Install platform build tools:
   - **macOS**: Xcode Command Line Tools (`xcode-select --install`)
   - **Windows**: "Desktop development with C++" workload from Visual Studio
     Build Tools, plus the WebView2 runtime (preinstalled on Windows 11).
3. `npm install`
4. Regenerate proper platform icons from the source art (placeholders are
   included, but this produces a correct multi-resolution `.icns`/`.ico`):
   ```
   npm run icon
   ```
5. Get `ffmpeg` **and** `ffprobe` for **your** platform and place them at
   `src-tauri/binaries/ffmpeg-<target-triple>` and
   `src-tauri/binaries/ffprobe-<target-triple>` (run `rustc -Vv | grep host`
   to find your triple). This is what the CI pipeline automates for release
   builds — locally you just need the pair for `tauri dev`/`tauri build`.

   On macOS, get them from `ffmpeg.martin-riedl.de`, **not** `evermeet.cx`:
   evermeet ships x86_64 only, so an Apple Silicon build made from it runs
   under Rosetta 2 and the neural denoiser becomes 3-5× slower. CI asserts
   the architecture with `lipo -archs` and fails the build on a mismatch.
6. `npm run dev` to run it locally, or `npm run build` to produce an
   installer in `src-tauri/target/release/bundle/`.

## How cleanup works

Presets are defined in `src-tauri/src/chain.rs` (one definition, covered by
tests) and the UI loads them via the `preset_options` command, so the Advanced
panel is always pre-populated from the chosen preset.

|  | Transcription / Diarization | Podcast | Light touch |
|---|---|---|---|
| high-pass | 70 Hz | 70 Hz | 70 Hz |
| denoise | `arnndn` mix 0.85 | `arnndn` mix 0.7 | off |
| level speakers | `speechnorm e=6:c=1` | `speechnorm e=4:c=2` | off |
| compression | off | gentle `acompressor` | off |
| loudness | one fixed gain, −19 LUFS | EBU R128, −16 LUFS | EBU R128, −16 LUFS |
| output | 16 kHz mono | 48 kHz, channels kept | source |
| ffmpeg passes | 2 (+ intermediate) | 2 (+ intermediate) | 2, no intermediate |

Chain order is repair → downmix → spectral → denoise → dynamics → rate
conversion → loudness → limiter. Several of those positions are load-bearing,
and the tests in `chain.rs` exist to keep them that way:

- **The rate conversion must follow `arnndn`**, which is hard-constrained to
  48 kHz. Place a 16 kHz downsample earlier and ffmpeg silently inserts an
  upsample back to 48 kHz: you lose the band, pay two resamples, and see no
  error.
- **It must also sit inside the measured segment.** Downsampling discards
  everything above the new Nyquist limit, which changes the loudness; measuring
  at 48 kHz and rendering at 16 kHz left a test file 3.2 dB off target.
- **The limiter is last of all.** Resampling and lossy encoding both overshoot,
  so a limiter placed earlier does not bound what reaches the file.
- **`alimiter` needs `level=false`.** It auto-levels *up* to the ceiling by
  default, which discards the computed gain — measured, the default pushed a
  −19 LUFS render to +0.2 dBFS.
- **`speechnorm` needs an explicit `c`.** It defaults to 2, i.e. 2× downward
  compression, flattening exactly the loud/quiet speaker differences that
  diarization depends on.
- **Hum notches need a high Q.** `t=q:w=1` reads as Q=1, roughly 60 Hz wide,
  which strips 10–23 dB of voice fundamental (male F0 is 85–180 Hz). The
  notches use `w=14`.

Two passes, but the expensive chain runs only **once**: pass A renders through
an `asplit` whose second branch is `loudnorm=print_format=json`, so the
measurement comes from the same render. Pass B then only applies gain and
encodes. When no denoiser is enabled the chain is cheap enough to run twice, so
Light touch skips the intermediate entirely.

The intermediate is 24-bit FLAC written at the final rate and channel count —
for a 16 kHz mono target that is a small fraction of the source, sidestepping
WAV's 4 GB ceiling on multi-hour files. It deletes itself on success, failure
and cancel, and a sweep clears orphans left by a hard kill.

**Deliberately absent:** silence trimming (`silenceremove` deletes samples, so
every timestamp after the first cut is wrong) and noise gating (`agate` clips
soft speech and inserts transients that VAD and speaker-change detection
misread as boundaries). `speechnorm` solves the level problem without either.

`adeclick`/`adeclip` are opt-in rather than preset defaults: `adeclick` adds
~68% to wall-clock time and targets vinyl/tape impulsive defects that Discord,
Craig and USB-mic recordings do not have.

## How conversion works

- The frontend lists files via native OS drag-and-drop or a file picker
  (`@tauri-apps/plugin-dialog`).
- `process_audio_file` (Rust) shells out to the bundled ffmpeg sidecar with a
  format-appropriate codec (e.g. `libmp3lame` for MP3, `libvorbis` for OGG),
  writing the output next to the source file and auto-numbering to avoid
  overwriting existing files. Cleaned output gets a ` - clean` suffix so it
  never shadows a plain conversion of the same source.
- Files are processed one at a time on purpose: the neural denoiser is
  CPU-bound, and several concurrent multi-hour jobs would thrash the machine.
- Progress is real, parsed from `-progress pipe:1`. Note `out_time_ms` is a
  misnomer that actually carries microseconds — the parser reads `out_time_us`.
- The `arnndn` model is passed as a **bare relative filename** with ffmpeg's
  working directory set to the model dir. An absolute path cannot be used:
  `:` and `\` are filtergraph metacharacters, so `C:\Program Files\...`
  fails to parse.
- Target formats are grouped in the dropdown by whether Archivist accepts them
  as a session upload, alphabetical within each group:
  - **Supported by Archivist**: M4A, MP3, Opus, WAV
  - **Not supported by Archivist**: AAC, AIFF, FLAC, OGG, WMA
  The grouping mirrors `src/lib/audio/contentType.js` in the Archivist app —
  update it here if the upload validator there changes. Archivist also accepts
  MP2 and WebM uploads, but this converter doesn't offer them as targets yet.
  Input format is unrestricted — ffmpeg reads almost anything.

## CI/CD: what happens on every push to `main`

`.github/workflows/release.yml` triggers on pushing a version tag
(`v*.*.*`) and runs a 3-way matrix build (`macos-14` → Apple Silicon,
`macos-15-intel` → Intel, `windows-latest`). For each platform it:

1. Downloads static `ffmpeg` **and** `ffprobe` builds and stages them as Tauri
   sidecar binaries.
2. On macOS, asserts each binary's architecture with `lipo -archs` and fails
   the build on a mismatch.
3. Smoke-tests the real cleanup filtergraph against the real bundled model, so
   an upstream ffmpeg option rename or a missing model fails here in seconds
   rather than in someone's 4-hour session.
4. Regenerates app icons from `icon-source.png`.
5. Runs [`tauri-apps/tauri-action`](https://github.com/tauri-apps/tauri-action)
   to build the app and publish installers as assets on a GitHub Release
   tagged with the pushed tag name.

To cut a release:
```
# bump the version in src-tauri/tauri.conf.json first, then:
git tag v0.2.1
git push --tags
```

### Things worth knowing / adjusting

- **Version consistency.** The git tag and `tauri.conf.json`'s `version`
  field aren't linked automatically — bump the latter before tagging so the
  built app reports the same version as the release.
- **Third-party ffmpeg builds.** The workflow pulls prebuilt static binaries
  from `gyan.dev` (Windows, the "essentials" zip, which contains both ffmpeg
  and ffprobe) and `ffmpeg.martin-riedl.de` (macOS, native arm64 and amd64) —
  long-running community projects, but not under your control. The macOS fetch
  retries on transient 404s. If a URL goes stale the fetch step fails with a
  clear download error; `BtbN/FFmpeg-Builds` on GitHub is a reasonable
  alternative.
- **Apple Silicon: this was a real bug, don't reintroduce it.** The workflow
  previously fetched from `evermeet.cx` for *both* macOS legs, but that project
  ships **x86_64 only** (`file` on the download reports
  `Mach-O 64-bit executable x86_64`) while the workflow staged it as
  `ffmpeg-aarch64-apple-darwin`. Apple Silicon users were therefore running
  ffmpeg under Rosetta 2. That was merely slow for remuxing; with the neural
  denoiser it turns a ~13-minute job into 30-60 minutes, or fails outright on a
  Mac without Rosetta. The `lipo -archs` assertion in CI is what now prevents
  this — do not remove it.
- **ffmpeg version drift.** The macOS source serves "latest" rather than a
  pinned version. Rather than pin a URL that will eventually 404, the CI smoke
  test runs the actual cleanup chain, which catches an option rename or a
  behaviour change immediately.
- **ffmpeg licensing.** These static builds typically include GPL-licensed
  components (e.g. libx264). Redistributing them means your app's
  distribution needs to comply with the GPL (e.g. making corresponding
  source available on request). If that's a problem, switch to an
  LGPL-only ffmpeg build (drop libx264/libx265/etc.) — fine for this app
  since none of the target formats need those codecs.
- **Code signing (macOS).** Release builds are signed and notarized when
  these repo secrets are set (same as
  `archivist-claude-podcast-installer`): `APPLE_CERTIFICATE`,
  `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`,
  `APPLE_PASSWORD`, and `APPLE_TEAM_ID`.
  - **Installing a signed release.** After downloading from GitHub, drag the
    app to Applications and open it once. macOS may show the usual Gatekeeper
    prompt that the developer cannot be verified (not the old misleading
    “damaged” message from unsigned downloads). Either right-click the app →
    **Open** → **Open**, or go to **System Settings → Privacy & Security**
    and use **Open Anyway**.
  - **Unsigned local or CI builds.** Without those secrets, Gatekeeper may
    still block the app (sometimes with a “damaged” error). For development
    only, you can clear quarantine with
    `xattr -cr "/Applications/Audio Converter.app"`.
