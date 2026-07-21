// Prevents an additional console window from appearing on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use serde::Serialize;
use tauri_plugin_shell::ShellExt;
use tauri_plugin_shell::process::CommandEvent;

#[derive(Serialize, Clone)]
struct ConversionResult {
    input_path: String,
    output_path: String,
}

#[derive(Serialize, Clone)]
struct ConversionError {
    input_path: String,
    message: String,
}

/// Builds a non-colliding output path next to the source file with the new extension.
fn build_output_path(input: &Path, target_ext: &str) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "converted".to_string());

    let mut candidate = parent.join(format!("{stem}.{target_ext}"));
    let mut counter = 1;
    while candidate.exists() {
        candidate = parent.join(format!("{stem} ({counter}).{target_ext}"));
        counter += 1;
    }
    candidate
}

#[tauri::command]
async fn convert_audio_file(
    app: tauri::AppHandle,
    input_path: String,
    target_format: String,
) -> Result<ConversionResult, ConversionError> {
    let input = PathBuf::from(&input_path);

    if !input.exists() {
        return Err(ConversionError {
            input_path: input_path.clone(),
            message: "File no longer exists on disk.".into(),
        });
    }

    let allowed = [
        "mp3", "wav", "flac", "ogg", "m4a", "aac", "wma", "opus", "aiff",
    ];
    let ext = target_format.to_lowercase();
    if !allowed.contains(&ext.as_str()) {
        return Err(ConversionError {
            input_path: input_path.clone(),
            message: format!("Unsupported target format: {target_format}"),
        });
    }

    let output = build_output_path(&input, &ext);

    // Sensible default codec per container so ffmpeg doesn't have to guess.
    let codec_args: Vec<&str> = match ext.as_str() {
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

    let mut args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        input.to_string_lossy().to_string(),
    ];
    args.extend(codec_args.iter().map(|s| s.to_string()));
    args.push(output.to_string_lossy().to_string());

    let sidecar = app
        .shell()
        .sidecar("ffmpeg")
        .map_err(|e| ConversionError {
            input_path: input_path.clone(),
            message: format!("Could not locate bundled ffmpeg: {e}"),
        })?
        .args(&args);

    let (mut rx, _child) = sidecar.spawn().map_err(|e| ConversionError {
        input_path: input_path.clone(),
        message: format!("Failed to start ffmpeg: {e}"),
    })?;

    let mut stderr_log = String::new();
    let mut succeeded = false;

    while let Some(event) = rx.recv().await {
        match event {
            CommandEvent::Stderr(line) => {
                stderr_log.push_str(&String::from_utf8_lossy(&line));
                stderr_log.push('\n');
            }
            CommandEvent::Terminated(payload) => {
                succeeded = payload.code == Some(0);
                break;
            }
            _ => {}
        }
    }

    if succeeded && output.exists() {
        Ok(ConversionResult {
            input_path,
            output_path: output.to_string_lossy().to_string(),
        })
    } else {
        // Keep the error message readable: last few lines of ffmpeg stderr.
        let tail: String = stderr_log
            .lines()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        Err(ConversionError {
            input_path,
            message: if tail.is_empty() {
                "ffmpeg exited with an error.".into()
            } else {
                tail
            },
        })
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![convert_audio_file])
        .run(tauri::generate_context!())
        .expect("error while running Audio Converter");
}
