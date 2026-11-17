//! Runs the bundled ffmpeg/ffprobe sidecars: progress events, loudness measurement, cancel.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;

use crate::chain::LoudnormStats;
use crate::jobs::JobRegistry;

/// Keeps the failure message useful without letting a multi-hour run accumulate an
/// unbounded log in memory.
const STDERR_KEEP_LINES: usize = 200;

/// How long a pass may run without `out_time` advancing before it is treated as hung.
///
/// FFmpeg 9.x livelocks in `arnndn` -> `speechnorm` on long speech-like input: it spins on
/// the CPU while producing no output, never errors and never exits (see README, "ffmpeg
/// version drift"). With no timeout the run loop waits on that forever - one user sat at
/// "Cleaning up 92%" for two days before giving up. The pin is the real fix; this only
/// guarantees the app fails loudly instead of freezing if a bad build ever ships again.
///
/// Deliberately generous. `-stats_period 0.5` makes a healthy pass report twice a second
/// however slow the machine, so five silent minutes is already far outside normal, while a
/// laptop that slept briefly mid-job still comes in under it.
const STALL_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the run loop wakes to check the watchdog when no events arrive at all.
const STALL_POLL: Duration = Duration::from_secs(5);

pub const PROGRESS_EVENT: &str = "cleanup://progress";

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPayload {
    pub input_path: String,
    /// Short label for the current pass, shown next to the bar.
    pub stage: String,
    /// Overall job completion, 0.0-1.0. `None` when the duration is unknown.
    pub fraction: Option<f64>,
    /// Realtime multiple ffmpeg reports, used for the ETA.
    pub speed: Option<f64>,
    pub eta_seconds: Option<f64>,
}

/// Where a pass sits within the whole job, so two passes produce one monotonic bar.
#[derive(Clone)]
pub struct ProgressCtx {
    pub input_path: String,
    pub stage: String,
    pub duration_us: Option<u64>,
    /// Fraction of the overall job this pass spans, e.g. `(0.0, 0.92)`.
    pub span: (f64, f64),
}

pub struct RunOutcome {
    pub success: bool,
    pub cancelled: bool,
    pub stderr_tail: String,
    pub loudnorm: Option<LoudnormStats>,
}

/// Arguments every invocation needs.
///
/// * `-nostdin` — the shell plugin hands the child a stdin pipe, and ffmpeg's interactive
///   key handling ("Press [q] to stop") can consume or block on it.
/// * `-nostats` — the periodic stats line is `\r`-terminated, and the plugin splits events
///   on `\r` as well as `\n`, so leaving it on floods the event channel.
/// * `-y` — never sit waiting on an overwrite prompt. Note ffmpeg exits **0** when it
///   declines to overwrite, so an exit code alone cannot prove the output was written.
fn base_args() -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-nostats".into(),
        "-y".into(),
    ]
}

/// Input-side arguments: take only the first audio stream.
///
/// Without `-map 0:a:0 -vn`, an mp3 or m4a carrying cover art has that artwork treated as a
/// video stream and transcoded into the output.
pub fn input_args(path: &str) -> Vec<String> {
    vec![
        "-i".into(),
        path.into(),
        "-map".into(),
        "0:a:0".into(),
        "-vn".into(),
        "-map_metadata".into(),
        "0".into(),
    ]
}

pub async fn run_ffmpeg(
    app: &AppHandle,
    jobs: &JobRegistry,
    job_id: &str,
    args: Vec<String>,
    model_dir: Option<PathBuf>,
    progress: Option<ProgressCtx>,
) -> Result<RunOutcome, String> {
    let mut full = base_args();
    if progress.is_some() {
        full.push("-progress".into());
        full.push("pipe:1".into());
        full.push("-stats_period".into());
        full.push("0.5".into());
    }
    full.extend(args);

    let mut cmd = app
        .shell()
        .sidecar("ffmpeg")
        .map_err(|e| format!("Could not locate the bundled ffmpeg: {e}"))?
        .args(&full);

    // `arnndn` takes its model as a bare filename because ':' and '\' are filtergraph
    // metacharacters — an absolute path like C:\Program Files\... fails to parse outright.
    // Running from the model directory sidesteps escaping entirely, on both platforms.
    if let Some(dir) = model_dir {
        cmd = cmd.current_dir(dir);
    }

    let (mut rx, child) = cmd
        .spawn()
        .map_err(|e| format!("Failed to start ffmpeg: {e}"))?;
    jobs.attach_child(job_id, child);

    let mut stderr: VecDeque<String> = VecDeque::with_capacity(STDERR_KEEP_LINES);
    let mut json = JsonBlockScanner::default();
    let mut prog = ProgressAccumulator::default();
    let mut success = false;
    let mut watch = StallWatch::new(Instant::now());
    let mut stalled = false;

    loop {
        // Waking on a timer instead of blocking on recv() is what makes a silent child
        // noticeable at all: a livelocked ffmpeg sends nothing and never exits, so the
        // event channel simply stays open and empty.
        let event = match tokio::time::timeout(STALL_POLL, rx.recv()).await {
            Ok(Some(event)) => Some(event),
            Ok(None) => break,
            Err(_) => None,
        };

        match event {
            Some(CommandEvent::Stdout(line)) => {
                let line = String::from_utf8_lossy(&line).to_string();
                if let Some(update) = prog.push(&line) {
                    watch.observe(update.out_time_us, Instant::now());
                    if let Some(ctx) = progress.as_ref() {
                        emit_progress(app, ctx, &update);
                    }
                }
            }
            Some(CommandEvent::Stderr(line)) => {
                let line = String::from_utf8_lossy(&line).to_string();
                json.push(&line);
                if stderr.len() == STDERR_KEEP_LINES {
                    stderr.pop_front();
                }
                stderr.push_back(line);
            }
            Some(CommandEvent::Terminated(payload)) => {
                success = payload.code == Some(0);
                break;
            }
            _ => {}
        }

        // Only a pass that asked for `-progress` reports out_time, so only such a pass can
        // be judged stalled. A cancelled job is already on its way down; leave it be.
        if progress.is_some() && watch.is_stalled(Instant::now()) {
            if jobs.is_cancelled(job_id) {
                break;
            }
            stalled = true;
            break;
        }
    }

    if stalled {
        if let Some(child) = jobs.take_child(job_id) {
            let _ = child.kill();
        }
        let stage = progress
            .as_ref()
            .map(|ctx| ctx.stage.as_str())
            .unwrap_or("This step");
        return Err(format!(
            "The \"{stage}\" step stopped responding: ffmpeg ran for {} minutes without \
             making any progress, so it was stopped. If this keeps happening, set Noise \
             reduction to Spectral or Off - some ffmpeg builds wedge in the neural \
             denoiser on long recordings.",
            STALL_TIMEOUT.as_secs() / 60
        ));
    }

    jobs.detach_child(job_id);
    let cancelled = jobs.is_cancelled(job_id);

    Ok(RunOutcome {
        success: success && !cancelled,
        cancelled,
        stderr_tail: stderr.iter().cloned().collect::<Vec<_>>().join("\n"),
        loudnorm: json.finish(),
    })
}

fn emit_progress(app: &AppHandle, ctx: &ProgressCtx, update: &ProgressUpdate) {
    let (start, end) = ctx.span;
    let fraction = ctx.duration_us.and_then(|total| {
        if total == 0 {
            return None;
        }
        // VBR mp3/m4a durations drift, so out_time can exceed the reported duration. Clamp
        // rather than treating it as an error.
        let within = (update.out_time_us as f64 / total as f64).clamp(0.0, 1.0);
        Some(start + within * (end - start))
    });

    let eta_seconds = match (ctx.duration_us, update.speed) {
        (Some(total), Some(speed)) if speed > 0.01 => {
            let remaining_us = total.saturating_sub(update.out_time_us) as f64;
            Some(remaining_us / 1_000_000.0 / speed)
        }
        _ => None,
    };

    let _ = app.emit(
        PROGRESS_EVENT,
        ProgressPayload {
            input_path: ctx.input_path.clone(),
            stage: ctx.stage.clone(),
            fraction,
            speed: update.speed,
            eta_seconds,
        },
    );
}

/// Decides whether a pass has stopped making progress.
///
/// Tracks the *high-water mark* of `out_time`, not the last value reported: a wedged ffmpeg
/// can keep emitting progress blocks that repeat or rewind out_time, and treating those as
/// signs of life would defeat the whole watchdog.
struct StallWatch {
    furthest_us: u64,
    last_advance: Instant,
}

impl StallWatch {
    fn new(now: Instant) -> Self {
        Self {
            furthest_us: 0,
            last_advance: now,
        }
    }

    fn observe(&mut self, out_time_us: u64, now: Instant) {
        if out_time_us > self.furthest_us {
            self.furthest_us = out_time_us;
            self.last_advance = now;
        }
    }

    fn is_stalled(&self, now: Instant) -> bool {
        now.duration_since(self.last_advance) >= STALL_TIMEOUT
    }
}

#[derive(Debug, PartialEq)]
struct ProgressUpdate {
    out_time_us: u64,
    speed: Option<f64>,
}

/// Collects `-progress` key/value lines until a block terminator.
///
/// ffmpeg writes one `key=value` per line, ending each block with `progress=continue` (or
/// `progress=end`). Audio-only jobs emit no `frame=`/`fps=`, so nothing may key off those.
#[derive(Default)]
struct ProgressAccumulator {
    out_time_us: Option<u64>,
    speed: Option<f64>,
}

impl ProgressAccumulator {
    /// Returns a value only at a block boundary.
    fn push(&mut self, line: &str) -> Option<ProgressUpdate> {
        let (key, value) = line.trim().split_once('=')?;
        let value = value.trim();
        match key.trim() {
            // `out_time_ms` is a long-standing misnomer: it carries MICROseconds, identical
            // to out_time_us. Reading it as milliseconds makes the bar finish 1000x early.
            "out_time_us" => self.out_time_us = value.parse().ok(),
            "speed" => {
                self.speed = value.trim_end_matches('x').trim().parse().ok();
            }
            "progress" => {
                let update = ProgressUpdate {
                    out_time_us: self.out_time_us.unwrap_or(0),
                    speed: self.speed,
                };
                self.speed = None;
                return Some(update);
            }
            _ => {}
        }
        None
    }
}

/// Pulls the `loudnorm` measurement block out of ffmpeg's stderr.
///
/// The block is preceded by a `[Parsed_loudnorm_N @ 0x…]` tag whose index shifts with the
/// shape of the filtergraph, so matching on the filter name is unreliable. Instead we look
/// for a line that is exactly `{` and accumulate to the matching `}`.
#[derive(Default)]
struct JsonBlockScanner {
    collecting: bool,
    buf: String,
    done: Option<LoudnormStats>,
}

impl JsonBlockScanner {
    fn push(&mut self, line: &str) {
        let trimmed = line.trim();
        if !self.collecting {
            if trimmed == "{" {
                self.collecting = true;
                self.buf.clear();
                self.buf.push('{');
            }
            return;
        }
        self.buf.push_str(trimmed);
        if trimmed == "}" {
            self.collecting = false;
            if let Some(stats) = parse_loudnorm(&self.buf) {
                self.done = Some(stats);
            }
        }
    }

    fn finish(self) -> Option<LoudnormStats> {
        self.done
    }
}

/// ffmpeg emits these numbers as JSON *strings* (`"input_i" : "-13.95"`), so they are read
/// as text and parsed rather than deserialized as numbers.
fn parse_loudnorm(raw: &str) -> Option<LoudnormStats> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let num = |key: &str| -> Option<f64> {
        let field = v.get(key)?;
        match field {
            serde_json::Value::String(s) => s.trim().parse().ok(),
            serde_json::Value::Number(n) => n.as_f64(),
            _ => None,
        }
    };
    Some(LoudnormStats {
        input_i: num("input_i")?,
        input_tp: num("input_tp")?,
        input_lra: num("input_lra")?,
        input_thresh: num("input_thresh")?,
    })
}

/// Media facts needed to turn progress into a percentage and to size the intermediate.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub duration_seconds: Option<f64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub codec: Option<String>,
}

impl MediaInfo {
    pub fn duration_us(&self) -> Option<u64> {
        self.duration_seconds
            .filter(|d| *d > 0.0)
            .map(|d| (d * 1_000_000.0) as u64)
    }
}

pub async fn probe(app: &AppHandle, input_path: &str) -> Result<MediaInfo, String> {
    let args: Vec<String> = vec![
        "-v".into(),
        "error".into(),
        "-print_format".into(),
        "json".into(),
        "-show_format".into(),
        "-show_streams".into(),
        "-select_streams".into(),
        "a:0".into(),
        input_path.into(),
    ];

    let output = app
        .shell()
        .sidecar("ffprobe")
        .map_err(|e| format!("Could not locate the bundled ffprobe: {e}"))?
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("Failed to run ffprobe: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "ffprobe could not read this file: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let v: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Could not parse ffprobe output: {e}"))?;

    let stream = v.get("streams").and_then(|s| s.get(0));
    let str_num = |val: Option<&serde_json::Value>| -> Option<f64> {
        match val? {
            serde_json::Value::String(s) => s.parse().ok(),
            serde_json::Value::Number(n) => n.as_f64(),
            _ => None,
        }
    };

    // Prefer the container duration; fall back to the stream's. Either may be absent, in
    // which case progress falls back to an indeterminate bar.
    let duration_seconds = str_num(v.get("format").and_then(|f| f.get("duration")))
        .or_else(|| str_num(stream.and_then(|s| s.get("duration"))));

    Ok(MediaInfo {
        duration_seconds,
        sample_rate: str_num(stream.and_then(|s| s.get("sample_rate"))).map(|r| r as u32),
        channels: str_num(stream.and_then(|s| s.get("channels"))).map(|c| c as u32),
        codec: stream
            .and_then(|s| s.get("codec_name"))
            .and_then(|c| c.as_str())
            .map(|s| s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_emits_once_per_block() {
        let mut acc = ProgressAccumulator::default();
        assert_eq!(acc.push("bitrate= 838.9kbits/s"), None);
        assert_eq!(acc.push("out_time_us=7500000"), None);
        assert_eq!(acc.push("out_time_ms=7500000"), None);
        assert_eq!(acc.push("speed=14.7x"), None);
        assert_eq!(
            acc.push("progress=continue"),
            Some(ProgressUpdate {
                out_time_us: 7_500_000,
                speed: Some(14.7)
            })
        );
    }

    #[test]
    fn progress_survives_missing_frame_keys() {
        // Audio-only jobs emit no frame=/fps=; a parser keyed on those would see nothing.
        let mut acc = ProgressAccumulator::default();
        acc.push("out_time_us=1000000");
        let got = acc.push("progress=end").expect("block should close");
        assert_eq!(got.out_time_us, 1_000_000);
    }

    #[test]
    fn progress_tolerates_na_values() {
        let mut acc = ProgressAccumulator::default();
        acc.push("bitrate=N/A");
        acc.push("total_size=N/A");
        acc.push("out_time_us=2000000");
        acc.push("speed=N/A");
        let got = acc.push("progress=continue").unwrap();
        assert_eq!(got.out_time_us, 2_000_000);
        assert_eq!(got.speed, None);
    }

    #[test]
    fn loudnorm_json_is_parsed_from_string_values() {
        let mut s = JsonBlockScanner::default();
        // Includes the tag line, whose filter index varies with graph shape.
        for line in [
            "[Parsed_loudnorm_4 @ 0x837029500] ",
            "{",
            "\t\"input_i\" : \"-24.33\",",
            "\t\"input_tp\" : \"-9.02\",",
            "\t\"input_lra\" : \"0.30\",",
            "\t\"input_thresh\" : \"-34.33\",",
            "\t\"output_i\" : \"-16.18\",",
            "\t\"normalization_type\" : \"dynamic\",",
            "\t\"target_offset\" : \"0.18\"",
            "}",
        ] {
            s.push(line);
        }
        let got = s.finish().expect("should have parsed a block");
        assert_eq!(got.input_i, -24.33);
        assert_eq!(got.input_tp, -9.02);
        assert_eq!(got.input_lra, 0.30);
        assert_eq!(got.input_thresh, -34.33);
    }

    #[test]
    fn scanner_ignores_unrelated_output() {
        let mut s = JsonBlockScanner::default();
        s.push("Input #0, wav, from 'x.wav':");
        s.push("  Duration: 00:00:12.00, bitrate: 768 kb/s");
        assert!(s.finish().is_none());
    }

    #[test]
    fn input_args_drop_cover_art() {
        let args = input_args("/tmp/a.mp3");
        assert!(args.windows(2).any(|w| w == ["-map", "0:a:0"]));
        assert!(args.contains(&"-vn".to_string()));
    }

    #[test]
    fn base_args_disable_stdin_and_stats() {
        let args = base_args();
        assert!(args.contains(&"-nostdin".to_string()));
        assert!(args.contains(&"-nostats".to_string()));
        assert!(args.contains(&"-y".to_string()));
    }

    #[test]
    fn tauri_runtime_has_a_timer() {
        // Tauri constructs its async runtime with `tokio::runtime::Runtime::new()` and does
        // not enable tokio's "time" feature itself; this crate's Cargo.toml does. If that
        // ever gets dropped, `enable_all()` quietly skips the timer and every ffmpeg pass
        // panics on its first `timeout` poll - a far worse failure than the hang this
        // watchdog exists to catch. Build the runtime the same way tauri does and prove the
        // timer actually fires.
        let rt = tokio::runtime::Runtime::new().expect("runtime should build");
        rt.block_on(async {
            let never = std::future::pending::<()>();
            let elapsed = tokio::time::timeout(Duration::from_millis(1), never).await;
            assert!(elapsed.is_err(), "timer driver is not running");
        });
    }

    #[test]
    fn stall_watch_resets_only_on_forward_motion() {
        let t0 = Instant::now();
        let mut watch = StallWatch::new(t0);
        watch.observe(5_000_000, t0 + Duration::from_secs(1));
        assert!(!watch.is_stalled(t0 + Duration::from_secs(100)));

        // A livelocked ffmpeg keeps reporting the same out_time. That is not progress, so
        // the clock must keep running from the last genuine advance.
        for tick in 2..60 {
            watch.observe(5_000_000, t0 + Duration::from_secs(tick * 5));
        }
        assert!(watch.is_stalled(t0 + Duration::from_secs(1) + STALL_TIMEOUT));
    }

    #[test]
    fn stall_watch_tolerates_a_rewound_out_time() {
        let t0 = Instant::now();
        let mut watch = StallWatch::new(t0);
        watch.observe(9_000_000, t0 + Duration::from_secs(10));
        watch.observe(1_000_000, t0 + Duration::from_secs(20)); // rewind: not progress
        assert!(watch.is_stalled(t0 + Duration::from_secs(10) + STALL_TIMEOUT));

        watch.observe(9_000_001, t0 + Duration::from_secs(30)); // past the mark: alive again
        assert!(!watch.is_stalled(t0 + Duration::from_secs(30) + STALL_TIMEOUT / 2));
    }

    #[test]
    fn stall_watch_covers_a_pass_that_never_starts() {
        // No progress block ever arrives (ffmpeg wedged before producing a sample), so the
        // window has to run from when the pass was spawned.
        let t0 = Instant::now();
        let watch = StallWatch::new(t0);
        assert!(!watch.is_stalled(t0 + STALL_TIMEOUT - Duration::from_secs(1)));
        assert!(watch.is_stalled(t0 + STALL_TIMEOUT));
    }

    #[test]
    fn duration_converts_to_microseconds() {
        let info = MediaInfo {
            duration_seconds: Some(12.5),
            sample_rate: None,
            channels: None,
            codec: None,
        };
        assert_eq!(info.duration_us(), Some(12_500_000));

        let unknown = MediaInfo {
            duration_seconds: None,
            ..info.clone()
        };
        assert_eq!(unknown.duration_us(), None);
    }
}
