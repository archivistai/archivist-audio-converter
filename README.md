# Audio Converter

A small Tauri desktop app: drag-and-drop (or browse for) audio files and
convert them to another format. Conversion is done by a bundled `ffmpeg`
binary ("sidecar") — no separate ffmpeg install required on the end user's
machine.

Stack: Tauri v2 (Rust backend) + plain HTML/CSS/JS frontend (no bundler
needed). GitHub Actions builds and releases signed-less installers for
macOS (Intel + Apple Silicon) and Windows on every push to `main`.

## Project layout

```
audio-converter/
├── src/                     # frontend (plain HTML/CSS/JS)
├── src-tauri/
│   ├── src/main.rs          # Rust commands (spawns the ffmpeg sidecar)
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
5. Get an `ffmpeg` binary for **your** platform and place it at
   `src-tauri/binaries/ffmpeg-<target-triple>` (see the CI workflow for the
   exact triples/filenames, or run `rustc -Vv | grep host` to find yours).
   This step is what the CI pipeline automates for release builds — locally
   you just need one binary for `tauri dev`/`tauri build` to work.
6. `npm run dev` to run it locally, or `npm run build` to produce an
   installer in `src-tauri/target/release/bundle/`.

## How conversion works

- The frontend lists files via native OS drag-and-drop or a file picker
  (`@tauri-apps/plugin-dialog`).
- `convert_audio_file` (Rust) shells out to the bundled ffmpeg sidecar with a
  format-appropriate codec (e.g. `libmp3lame` for MP3, `libvorbis` for OGG),
  writing the output next to the source file and auto-numbering to avoid
  overwriting existing files.
- Supported target formats: MP3, WAV, FLAC, OGG, Opus, M4A, AAC, WMA, AIFF.
  Input format is unrestricted — ffmpeg reads almost anything.

## CI/CD: what happens on every push to `main`

`.github/workflows/release.yml` triggers on pushing a version tag
(`v*.*.*`) and runs a 3-way matrix build (`macos-14` → Apple Silicon,
`macos-13` → Intel, `windows-latest`). For each platform it:

1. Downloads a static `ffmpeg` build and stages it as the Tauri sidecar
   binary.
2. Regenerates app icons from `icon-source.png`.
3. Runs [`tauri-apps/tauri-action`](https://github.com/tauri-apps/tauri-action)
   to build the app and publish installers as assets on a GitHub Release
   tagged with the pushed tag name.

To cut a release:
```
# bump the version in src-tauri/tauri.conf.json first, then:
git tag v0.1.0
git push --tags
```

### Things worth knowing / adjusting

- **Version consistency.** The git tag and `tauri.conf.json`'s `version`
  field aren't linked automatically — bump the latter before tagging so the
  built app reports the same version as the release.
- **Third-party ffmpeg builds.** The workflow pulls prebuilt static ffmpeg
  binaries from `gyan.dev` (Windows) and `evermeet.cx` (macOS) — long-running
  community projects, but not under Anthropic's or your control. If a URL
  ever goes stale, the "Fetch ffmpeg" step will fail with a clear download
  error; swap in a different static-build source (e.g.
  `https://www.osxexperts.net` for native arm64 macOS builds, or
  `BtbN/FFmpeg-Builds` on GitHub for Windows/Linux) in that step.
- **Apple Silicon note.** The macOS ffmpeg binary used here is a single
  build from evermeet.cx; if at some point it's Intel-only rather than
  universal, it'll still run on Apple Silicon Macs via Rosetta 2 (usually
  auto-installed on first launch of an Intel binary). If you want to avoid
  that prompt entirely, source a native `arm64` static build instead for the
  `macos-14` leg.
- **ffmpeg licensing.** These static builds typically include GPL-licensed
  components (e.g. libx264). Redistributing them means your app's
  distribution needs to comply with the GPL (e.g. making corresponding
  source available on request). If that's a problem, switch to an
  LGPL-only ffmpeg build (drop libx264/libx265/etc.) — fine for this app
  since none of the target formats need those codecs.
- **Code signing.** Builds are unsigned. macOS users will need to
  right-click → Open (or clear the quarantine flag) on first launch, and
  Windows SmartScreen may warn on first run. Add
  `APPLE_CERTIFICATE`/`APPLE_CERTIFICATE_PASSWORD` (or a Windows code-signing
  cert) as repo secrets and pass them to `tauri-action` when you're ready to
  sign.
