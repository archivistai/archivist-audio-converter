// Prevents an additional console window from appearing on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chain;
mod ffmpeg;
mod jobs;
mod paths;

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::Manager;

use chain::{CleanupOptions, LoudnormStats};
use ffmpeg::{input_args, MediaInfo, ProgressCtx};
use jobs::JobRegistry;
use paths::{build_output_path, TempFile};

/// Containers we can write. Input is unrestricted — ffmpeg reads nearly anything.
const ALLOWED_FORMATS: [&str; 9] = [
    "mp3", "wav", "flac", "ogg", "m4a", "aac", "wma", "opus", "aiff",
];

/// Distinguishes cleaned output from a plain conversion of the same source.
const CLEAN_SUFFIX: &str = " - clean";

/// Pass A carries essentially all the cost, so the bar reflects that rather than splitting
/// evenly and appearing to stall.
const PASS_A_SHARE: f64 = 0.92;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ProcessResult {
    input_path: String,
    output_path: String,
    /// Loudness after cleanup but before gain — the "before" half of the report.
    measured_lufs: Option<f64>,
    applied_gain_db: Option<f64>,
    target_lufs: Option<f64>,
    duration_seconds: Option<f64>,
    cancelled: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ConversionError {
    input_path: String,
    message: String,
}

impl ConversionError {
    fn new(input_path: &str, message: impl Into<String>) -> Self {
        Self {
            input_path: input_path.to_string(),
            message: message.into(),
        }
    }
}

/// Turns an ffmpeg stderr tail into something a user can act on.
fn explain_failure(stderr_tail: &str) -> String {
    // A missing or malformed arnndn model reports only "Error initializing filters" with no
    // mention of the filter or the path, which is impossible to act on as-is.
    if stderr_tail.contains("Error initializing filters")
        || stderr_tail.contains("Invalid data found when processing input")
    {
        if stderr_tail.contains("arnndn") || stderr_tail.contains("filterchain") {
            return "The noise-reduction model could not be loaded. Try reinstalling the app, \
                    or set Noise reduction to Spectral or Off."
                .into();
        }
    }
    if stderr_tail.contains("No space left on device") {
        return "Ran out of disk space. Cleanup writes a temporary file next to the output; \
                free some space and try again."
            .into();
    }
    let tail: Vec<&str> = stderr_tail.lines().rev().take(6).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    if tail.is_empty() {
        "ffmpeg exited with an error.".into()
    } else {
        tail.join("\n")
    }
}

/// Default codec per container, so ffmpeg doesn't have to guess.
fn codec_args(ext: &str) -> Vec<String> {
    let args: Vec<&str> = match ext {
        "mp3" => vec!["-c:a", "libmp3lame", "-q:a", "2"],
        "aac" | "m4a" => vec!["-c:a", "aac", "-b:a", "192k"],
        "ogg" => vec!["-c:a", "libvorbis", "-q:a", "5"],
        "opus" => vec!["-c:a", "libopus", "-b:a", "128k"],
        "flac" => vec!["-c:a", "flac"],
        "wav" => vec!["-c:a", "pcm_s16le"],
        "wma" => vec!["-c:a", "wmav2"],
        "aiff" => vec!["-c:a", "pcm_s16be"],
        _ => vec![],
    };
    args.into_iter().map(String::from).collect()
}

/// Locates the bundled RNNoise model's directory. ffmpeg runs with this as its working
/// directory so the model can be named relatively — see `run_ffmpeg`.
fn model_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let resolved = app
        .path()
        .resolve("models", tauri::path::BaseDirectory::Resource)
        .ok()?;
    if resolved.join(chain::RNNOISE_MODEL).exists() {
        return Some(resolved);
    }
    // `tauri dev` runs from src-tauri, where the model sits un-bundled.
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models");
    dev.join(chain::RNNOISE_MODEL).exists().then_some(dev)
}

fn filters_to_arg(filters: &[String]) -> String {
    filters.join(",")
}

/// Returns a preset's settings so the UI can populate the Advanced panel from it.
///
/// Presets live here rather than in the frontend so they have one definition and stay
/// covered by the chain tests.
#[tauri::command]
fn preset_options(name: String) -> Result<CleanupOptions, String> {
    Ok(match name.as_str() {
        "none" => CleanupOptions::default(),
        "transcription" => CleanupOptions::transcription(),
        "podcast" => CleanupOptions::podcast(),
        "light" => CleanupOptions::light_touch(),
        other => return Err(format!("Unknown preset: {other}")),
    })
}

#[tauri::command]
async fn probe_audio_file(
    app: tauri::AppHandle,
    input_path: String,
) -> Result<MediaInfo, String> {
    ffmpeg::probe(&app, &input_path).await
}

#[tauri::command]
fn cancel_job(jobs: tauri::State<'_, JobRegistry>, job_id: String) -> Result<(), String> {
    // The flag is set inside `cancel` before the child is handed over, so the run loop can
    // tell this apart from a crash.
    if let Some(child) = jobs.cancel(&job_id)? {
        child.kill().map_err(|e| format!("Could not stop ffmpeg: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
async fn process_audio_file(
    app: tauri::AppHandle,
    jobs: tauri::State<'_, JobRegistry>,
    input_path: String,
    target_format: String,
    cleanup: CleanupOptions,
) -> Result<ProcessResult, ConversionError> {
    let input = PathBuf::from(&input_path);
    if !input.exists() {
        return Err(ConversionError::new(
            &input_path,
            "File no longer exists on disk.",
        ));
    }

    let ext = target_format.to_lowercase();
    if !ALLOWED_FORMATS.contains(&ext.as_str()) {
        return Err(ConversionError::new(
            &input_path,
            format!("Unsupported target format: {target_format}"),
        ));
    }

    let suffix = if cleanup.enabled { CLEAN_SUFFIX } else { "" };
    let output = build_output_path(&input, suffix, &ext);

    // Duration drives the progress percentage. A file ffprobe can't measure still converts;
    // the bar just goes indeterminate.
    let info = ffmpeg::probe(&app, &input_path).await.ok();
    let duration_us = info.as_ref().and_then(MediaInfo::duration_us);
    let duration_seconds = info.as_ref().and_then(|i| i.duration_seconds);

    let job_id = input_path.clone();
    jobs.register(&job_id);
    let result = run_job(
        &app,
        &jobs,
        &job_id,
        &input_path,
        &output,
        &ext,
        &cleanup,
        duration_us,
        duration_seconds,
    )
    .await;
    jobs.finish(&job_id);

    result
}

#[allow(clippy::too_many_arguments)]
async fn run_job(
    app: &tauri::AppHandle,
    jobs: &JobRegistry,
    job_id: &str,
    input_path: &str,
    output: &Path,
    ext: &str,
    cleanup: &CleanupOptions,
    duration_us: Option<u64>,
    duration_seconds: Option<f64>,
) -> Result<ProcessResult, ConversionError> {
    let dir = model_dir(app);
    // Processing plus the output rate conversion: everything that must happen before the
    // loudness reading is taken, so the reading reflects the audio that actually ships.
    let pre_loudness = cleanup.pre_loudness_filters();

    let mut measured: Option<LoudnormStats> = None;
    let mut applied_gain_db: Option<f64> = None;

    // Where the render reads from: either the original input, or an intermediate holding the
    // already-processed audio.
    let mut source = input_path.to_string();
    // Filters still to apply at render time. Emptied when the intermediate already has them.
    let mut remaining = pre_loudness.clone();
    let mut stage: Option<TempFile> = None;

    if cleanup.needs_measurement() {
        if cleanup.needs_intermediate() {
            // The denoiser is far too costly to run twice, so render once into an
            // intermediate while measuring the same graph through an `asplit` branch.
            //
            // FLAC rather than WAV: ~60% the size, no 4 GB container limit, negligible
            // encode cost against the chain, and 24-bit keeps headroom for the gain-up.
            // It already carries the final rate and channel count, so for a 16 kHz mono
            // target it is a small fraction of the source size.
            // Clear any intermediate a previous hard kill left behind here before adding
            // another; `Drop` cannot run when the process is killed outright.
            if let Some(parent) = output.parent() {
                paths::sweep_orphans(parent);
            }
            let temp = TempFile::beside(output, "flac");
            let graph = format!(
                "[0:a]{},asplit=2[a][b];[b]{}[m]",
                filters_to_arg(&pre_loudness),
                cleanup.measurement_filter()
            );
            // Explicit filtergraph labels replace the `-map 0:a:0 -vn` form used elsewhere.
            let args = vec![
                "-i".to_string(),
                input_path.to_string(),
                "-filter_complex".to_string(),
                graph,
                "-map".to_string(),
                "[a]".to_string(),
                "-c:a".to_string(),
                "flac".to_string(),
                "-sample_fmt".to_string(),
                "s32".to_string(),
                "-f".to_string(),
                "flac".to_string(),
                temp.path().to_string_lossy().to_string(),
                "-map".to_string(),
                "[m]".to_string(),
                "-f".to_string(),
                "null".to_string(),
                "-".to_string(),
            ];

            let outcome = run_pass(
                app,
                jobs,
                job_id,
                args,
                dir.clone(),
                input_path,
                "Cleaning up",
                duration_us,
                (0.0, PASS_A_SHARE),
            )
            .await?;
            if let Some(r) = cancelled_result(&outcome, input_path, output, duration_seconds) {
                return r;
            }

            measured = outcome.loudnorm;
            source = temp.path().to_string_lossy().to_string();
            remaining.clear();
            stage = Some(temp);
        } else {
            // Nothing expensive in the chain, so it is cheaper overall to measure in a
            // throwaway pass and render in one go — no intermediate, no disk-space risk.
            let mut args = input_args(input_path);
            let mut graph = pre_loudness.clone();
            graph.push(cleanup.measurement_filter());
            args.push("-af".into());
            args.push(filters_to_arg(&graph));
            args.push("-f".into());
            args.push("null".into());
            args.push("-".into());

            let outcome = run_pass(
                app,
                jobs,
                job_id,
                args,
                dir.clone(),
                input_path,
                "Analyzing",
                duration_us,
                (0.0, 0.5),
            )
            .await?;
            if let Some(r) = cancelled_result(&outcome, input_path, output, duration_seconds) {
                return r;
            }
            measured = outcome.loudnorm;
        }

        if measured.is_none() {
            return Err(ConversionError::new(
                input_path,
                "Could not measure the loudness of this file, so it was left untouched.",
            ));
        }
    }

    // --- Render pass -------------------------------------------------------------------
    let mut render = remaining;
    if let Some(m) = measured.as_ref() {
        if cleanup.loudness == chain::LoudnessMode::Static {
            applied_gain_db = Some(cleanup.static_gain_db(m));
        }
        render.extend(cleanup.loudness_filters(m));
    }
    // The limiter is last of all: resampling and lossy encoding both overshoot, so a
    // limiter placed earlier would not bound what reaches the file.
    render.extend(cleanup.limiter_filters());

    // Render to a staged file and rename on success, so a cancel can never leave a
    // truncated file sitting at the final path looking complete.
    let final_stage = TempFile::beside(output, ext);
    let mut args = input_args(&source);
    if !render.is_empty() {
        args.push("-af".into());
        args.push(filters_to_arg(&render));
    }
    args.extend(codec_args(ext));
    args.push("-f".into());
    args.push(container_format(ext).into());
    args.push(final_stage.path().to_string_lossy().to_string());

    let span = if cleanup.needs_measurement() {
        if cleanup.needs_intermediate() {
            (PASS_A_SHARE, 1.0)
        } else {
            (0.5, 1.0)
        }
    } else {
        (0.0, 1.0)
    };
    let outcome = run_pass(
        app,
        jobs,
        job_id,
        args,
        dir,
        input_path,
        "Encoding",
        duration_us,
        span,
    )
    .await?;
    if let Some(r) = cancelled_result(&outcome, input_path, output, duration_seconds) {
        return r;
    }

    // ffmpeg exits 0 when it declines to overwrite, so the exit code alone cannot prove the
    // file was written. Check it really exists and has content.
    let wrote_output = std::fs::metadata(final_stage.path())
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if !outcome.success || !wrote_output {
        return Err(ConversionError::new(
            input_path,
            explain_failure(&outcome.stderr_tail),
        ));
    }

    let mut final_stage = final_stage;
    std::fs::rename(final_stage.path(), output).map_err(|e| {
        ConversionError::new(input_path, format!("Could not save the converted file: {e}"))
    })?;
    final_stage.keep();
    drop(stage); // Removes the intermediate now rather than at end of scope.

    Ok(ProcessResult {
        input_path: input_path.to_string(),
        output_path: output.to_string_lossy().to_string(),
        measured_lufs: measured.map(|m| m.input_i),
        applied_gain_db,
        target_lufs: cleanup
            .needs_measurement()
            .then_some(cleanup.target_lufs),
        duration_seconds,
        cancelled: false,
    })
}

/// Container name for `-f`, where it differs from the extension.
fn container_format(ext: &str) -> &str {
    match ext {
        "m4a" => "ipod",
        "aac" => "adts",
        "ogg" => "ogg",
        "opus" => "opus",
        "wma" => "asf",
        "aiff" => "aiff",
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_pass(
    app: &tauri::AppHandle,
    jobs: &JobRegistry,
    job_id: &str,
    args: Vec<String>,
    model_dir: Option<PathBuf>,
    input_path: &str,
    stage: &str,
    duration_us: Option<u64>,
    span: (f64, f64),
) -> Result<ffmpeg::RunOutcome, ConversionError> {
    ffmpeg::run_ffmpeg(
        app,
        jobs,
        job_id,
        args,
        model_dir,
        Some(ProgressCtx {
            input_path: input_path.to_string(),
            stage: stage.to_string(),
            duration_us,
            span,
        }),
    )
    .await
    .map_err(|e| ConversionError::new(input_path, e))
}

/// Short-circuits the pipeline when the user cancelled, without treating it as a failure.
fn cancelled_result(
    outcome: &ffmpeg::RunOutcome,
    input_path: &str,
    output: &Path,
    duration_seconds: Option<f64>,
) -> Option<Result<ProcessResult, ConversionError>> {
    outcome.cancelled.then(|| {
        Ok(ProcessResult {
            input_path: input_path.to_string(),
            output_path: output.to_string_lossy().to_string(),
            measured_lufs: None,
            applied_gain_db: None,
            target_lufs: None,
            duration_seconds,
            cancelled: true,
        })
    })
}

/// Kept so the original conversion path stays available unchanged.
#[tauri::command]
async fn convert_audio_file(
    app: tauri::AppHandle,
    jobs: tauri::State<'_, JobRegistry>,
    input_path: String,
    target_format: String,
) -> Result<ProcessResult, ConversionError> {
    process_audio_file(
        app,
        jobs,
        input_path,
        target_format,
        CleanupOptions::default(),
    )
    .await
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(JobRegistry::default())
        .invoke_handler(tauri::generate_handler![
            convert_audio_file,
            process_audio_file,
            probe_audio_file,
            preset_options,
            cancel_job
        ])
        .run(tauri::generate_context!())
        .expect("error while running Audio Converter");
}
