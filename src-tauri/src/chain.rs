//! Builds ffmpeg filter chains for speech cleanup.
//!
//! Pure string building — no I/O, no ffmpeg — so the ordering invariants that matter can be
//! unit-tested without a sidecar. Those invariants are not cosmetic; each one below was a
//! real bug caught by measurement:
//!
//! * The final downsample must come *after* `arnndn`, which is hard-constrained to 48 kHz.
//!   Put it earlier and ffmpeg silently auto-inserts a resample back up to 48 kHz, so you
//!   lose the band, pay two resamples, and see no error.
//! * Hum notches need a high Q. `t=q:w=1` reads as Q=1 (~60 Hz of bandwidth), which removes
//!   10-23 dB of voice fundamental — precisely the cue speaker-embedding models rely on.
//! * `speechnorm`'s `compression` defaults to 2, so omitting it applies 2x downward
//!   compression that flattens the loud/quiet speaker difference we are trying to preserve.

use serde::{Deserialize, Serialize};

/// Filename of the bundled RNNoise model ("somnolent-hogwash": trained on recording noise
/// with a speech signal). Passed to `arnndn` as a bare relative name — see `model_dir` on
/// the ffmpeg runner for why an absolute path is not an option.
pub const RNNOISE_MODEL: &str = "sh.rnnn";


#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DenoiseKind {
    #[default]
    Off,
    /// `afftdn` — spectral gate. Cheap (~600x realtime) and model-free, but leaves more
    /// residue than the neural denoiser.
    Spectral,
    /// `arnndn` — RNNoise. Much better on speech, markedly slower.
    Neural,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LoudnessMode {
    #[default]
    Off,
    /// One measured gain applied to the whole file (`volume` + `alimiter`). Always linear,
    /// so it preserves relative speaker levels. The right choice for diarization.
    Static,
    /// EBU R128 via two-pass `loudnorm`. Hits a broadcast target more exactly, but only
    /// stays linear when the measured LRA fits inside the target.
    Ebu,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CleanupOptions {
    pub enabled: bool,

    // Repair. Both are off by default: `adeclick` is an autoregressive interpolator for
    // vinyl/tape impulsive defects and costs ~68% extra wall time on the full chain, which
    // buys nothing on Discord/Craig/USB-mic material.
    pub declick: bool,
    pub declip: bool,

    /// High-pass corner in Hz; 0 disables. 70 Hz clears rumble and HVAC while leaving the
    /// lowest male fundamentals (~85 Hz) essentially intact.
    pub highpass_hz: u32,
    /// Mains hum fundamental: 50 (most of the world), 60 (North America), or 0 for off.
    /// Notches the fundamental and its 2nd and 3rd harmonics.
    pub hum_hz: u32,

    pub denoise: DenoiseKind,
    /// `arnndn` wet/dry mix. Below 1.0 deliberately leaves some room tone: RNNoise is
    /// VAD-driven and tends to suppress quiet overlapping speech, which is exactly the
    /// signal overlap detection needs.
    pub denoise_mix: f64,
    /// `afftdn` noise reduction in dB.
    pub denoise_nr: f64,

    /// `speechnorm` — the filter that actually lifts quiet speakers.
    pub speechnorm: bool,
    /// Max upward expansion factor (1-50).
    pub speechnorm_expansion: f64,
    /// Max downward compression factor. 1.0 = none; keep it there to preserve the
    /// loud/quiet speaker difference.
    pub speechnorm_compression: f64,

    /// Gentle `acompressor`, for listenable/podcast output rather than ASR.
    pub compressor: bool,

    pub loudness: LoudnessMode,
    pub target_lufs: f64,
    pub target_tp: f64,
    /// Only meaningful for `LoudnessMode::Ebu`.
    pub target_lra: f64,

    /// Output sample rate; 0 keeps the source rate. 16000 is what Whisper and pyannote want.
    pub output_rate: u32,
    /// Downmix to a single channel.
    pub mono: bool,
}

impl Default for CleanupOptions {
    fn default() -> Self {
        // Cleanup disabled: a plain format conversion, matching the app's original behaviour.
        Self {
            enabled: false,
            declick: false,
            declip: false,
            highpass_hz: 0,
            hum_hz: 0,
            denoise: DenoiseKind::Off,
            denoise_mix: 0.85,
            denoise_nr: 10.0,
            speechnorm: false,
            speechnorm_expansion: 6.0,
            speechnorm_compression: 1.0,
            compressor: false,
            loudness: LoudnessMode::Off,
            target_lufs: -16.0,
            target_tp: -1.5,
            target_lra: 20.0,
            output_rate: 0,
            mono: false,
        }
    }
}

impl CleanupOptions {
    pub fn transcription() -> Self {
        Self {
            enabled: true,
            highpass_hz: 70,
            denoise: DenoiseKind::Neural,
            denoise_mix: 0.85,
            speechnorm: true,
            speechnorm_expansion: 6.0,
            speechnorm_compression: 1.0,
            // Static gain, not loudnorm: deterministic and always linear, so the relative
            // level differences between speakers survive.
            loudness: LoudnessMode::Static,
            target_lufs: -19.0,
            target_tp: -3.0,
            output_rate: 16_000,
            mono: true,
            ..Default::default()
        }
    }

    pub fn podcast() -> Self {
        Self {
            enabled: true,
            highpass_hz: 70,
            denoise: DenoiseKind::Neural,
            denoise_mix: 0.7,
            speechnorm: true,
            speechnorm_expansion: 4.0,
            speechnorm_compression: 2.0,
            compressor: true,
            loudness: LoudnessMode::Ebu,
            target_lufs: -16.0,
            target_tp: -1.5,
            // 20 rather than the broadcast-typical 11: a multi-hour session easily measures
            // LRA 15-20, and loudnorm silently drops to dynamic (pumping) mode whenever the
            // measured LRA exceeds the target.
            target_lra: 20.0,
            output_rate: 48_000,
            ..Default::default()
        }
    }

    pub fn light_touch() -> Self {
        Self {
            enabled: true,
            highpass_hz: 70,
            loudness: LoudnessMode::Ebu,
            target_lufs: -16.0,
            target_tp: -1.5,
            target_lra: 20.0,
            ..Default::default()
        }
    }

    /// Everything up to (but excluding) loudness correction and the final rate/channel
    /// conversion. This is the segment whose output gets measured.
    pub fn processing_filters(&self) -> Vec<String> {
        let mut f: Vec<String> = Vec::new();
        if !self.enabled {
            return f;
        }

        if self.declip {
            f.push("adeclip".into());
        }
        if self.declick {
            f.push("adeclick".into());
        }
        // Downmix before the denoiser, not after: arnndn processes each channel
        // independently, so collapsing to mono first halves both the denoise work and the
        // size of the intermediate file.
        if self.mono {
            f.push("aformat=channel_layouts=mono".into());
        }
        if self.highpass_hz > 0 {
            f.push(format!("highpass=f={}:poles=2", self.highpass_hz));
        }
        if self.hum_hz > 0 {
            // Q=14 keeps each notch about 4 Hz wide. Verified: -26 dB at 60 Hz but only
            // -3 dB at 80 Hz, versus -23 dB at 80 Hz with the Q=1 default.
            for (mult, gain) in [(1, -24), (2, -18), (3, -12)] {
                f.push(format!(
                    "equalizer=f={}:t=q:w=14:g={}",
                    self.hum_hz * mult,
                    gain
                ));
            }
        }

        match self.denoise {
            DenoiseKind::Off => {}
            DenoiseKind::Spectral => {
                // tn=1 tracks a drifting noise floor rather than assuming a fixed profile.
                f.push(format!("afftdn=nr={}:nf=-25:tn=1", fmt_num(self.denoise_nr)));
            }
            DenoiseKind::Neural => {
                // arnndn only accepts 48 kHz. ffmpeg would insert this automatically; being
                // explicit documents the constraint and keeps the graph predictable.
                f.push("aresample=48000".into());
                f.push(format!(
                    "arnndn=m={}:mix={}",
                    RNNOISE_MODEL,
                    fmt_num(self.denoise_mix)
                ));
            }
        }

        if self.speechnorm {
            f.push(format!(
                "speechnorm=e={}:c={}:r=0.0001:l=1",
                fmt_num(self.speechnorm_expansion),
                fmt_num(self.speechnorm_compression)
            ));
        }
        if self.compressor {
            f.push("acompressor=threshold=-18dB:ratio=3:attack=20:release=250:makeup=2".into());
        }

        f
    }

    /// Peak safety net, emitted *after* the sample-rate conversion.
    ///
    /// Position matters: resampling and lossy encoding both overshoot, so a limiter placed
    /// before them does not constrain what actually reaches the file. Measured on a test
    /// render, limiting at 48 kHz ahead of a 16 kHz downsample and an mp3 encode let the
    /// peak come back 1.9 dB above the ceiling.
    ///
    /// Only needed in `Static` mode; `loudnorm` enforces its own true-peak target.
    pub fn limiter_filters(&self) -> Vec<String> {
        if !self.enabled || self.loudness != LoudnessMode::Static {
            return vec![];
        }
        // level=false is essential: `alimiter` defaults to auto-levelling, which normalizes
        // the signal UP to the ceiling. Measured, the default pushed a -19 LUFS render to
        // +0.2 dBFS — clipping — and would override the computed gain entirely.
        vec![format!(
            "alimiter=limit={}:level=false",
            fmt_num(db_to_linear(self.target_tp))
        )]
    }

    /// Final sample-rate conversion. Must come after `arnndn`, which is locked to 48 kHz, so
    /// a downsample placed anywhere before it gets silently undone.
    pub fn output_format_filters(&self) -> Vec<String> {
        let mut f = Vec::new();
        if !self.enabled {
            return f;
        }
        if self.output_rate > 0 {
            f.push(format!("aresample={}", self.output_rate));
        }
        f
    }

    /// Everything that must run before loudness is measured: processing plus the final rate
    /// conversion.
    ///
    /// The rate conversion belongs *inside* the measured segment. Downsampling discards
    /// everything above the new Nyquist limit, which changes the loudness — measuring at
    /// 48 kHz and then rendering at 16 kHz left a noisy test file 3.2 dB below the predicted
    /// target. Measuring after the resample also means the intermediate is written at the
    /// final rate and channel count, which is roughly a sixth of the size for 16 kHz mono.
    pub fn pre_loudness_filters(&self) -> Vec<String> {
        let mut f = self.processing_filters();
        f.extend(self.output_format_filters());
        f
    }

    /// Whether loudness correction needs a measurement pass at all.
    pub fn needs_measurement(&self) -> bool {
        self.enabled && self.loudness != LoudnessMode::Off
    }

    /// Whether the processing chain is costly enough that it must not be run twice.
    ///
    /// When true, the render is done once into a temp file with the measurement taken from
    /// an `asplit` branch of the same graph. When false, it's cheaper overall to measure in
    /// a throwaway pass and render in one go — no temp file, no disk-space risk.
    pub fn needs_intermediate(&self) -> bool {
        self.needs_measurement() && self.denoise != DenoiseKind::Off
    }

    /// The loudness filters for the render pass, given the measurement.
    pub fn loudness_filters(&self, m: &LoudnormStats) -> Vec<String> {
        match self.loudness {
            LoudnessMode::Off => vec![],
            LoudnessMode::Static => {
                let gain = self.static_gain_db(m);
                if gain.abs() < 0.05 {
                    return vec![];
                }
                vec![format!("volume={}dB", fmt_num(gain))]
            }
            LoudnessMode::Ebu => vec![format!(
                "loudnorm=I={}:TP={}:LRA={}:measured_I={}:measured_TP={}:measured_LRA={}:measured_thresh={}:linear=true",
                fmt_num(self.target_lufs),
                fmt_num(self.target_tp),
                fmt_num(self.target_lra),
                fmt_num(m.input_i),
                fmt_num(m.input_tp),
                fmt_num(m.input_lra),
                fmt_num(m.input_thresh),
            )],
        }
    }

    /// Gain that reaches the loudness target without driving true peak into distortion.
    ///
    /// Ordinary speech sits 12-16 dB below its peaks, so the loudness target usually governs
    /// and the peak term rarely binds. Deliberately no "push past the ceiling and let the
    /// limiter catch it" allowance: measured on peaky material, gain added beyond the peak
    /// budget is simply removed again by the limiter, landing on the same loudness with more
    /// distortion. Falling short of the target is reported honestly instead.
    pub fn static_gain_db(&self, m: &LoudnormStats) -> f64 {
        let by_loudness = self.target_lufs - m.input_i;
        let by_peak = self.target_tp - m.input_tp;
        by_loudness.min(by_peak).clamp(-30.0, 30.0)
    }

    /// `loudnorm` measurement arguments. Target values matter here only because they decide
    /// the reported `normalization_type`; the `input_*` readings are what we consume.
    pub fn measurement_filter(&self) -> String {
        format!(
            "loudnorm=I={}:TP={}:LRA={}:print_format=json",
            fmt_num(self.target_lufs),
            fmt_num(self.target_tp),
            fmt_num(self.target_lra)
        )
    }
}

/// The `input_*` readings from a `loudnorm` measurement pass.
///
/// ffmpeg prints these as JSON *strings*, not numbers, so they are parsed in `ffmpeg.rs`
/// rather than deserialized directly.
#[derive(Serialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct LoudnormStats {
    pub input_i: f64,
    pub input_tp: f64,
    pub input_lra: f64,
    pub input_thresh: f64,
}

fn db_to_linear(db: f64) -> f64 {
    (10f64).powf(db / 20.0)
}

/// Formats a float for a filtergraph: trims trailing zeros so `6.0` reads as `6`, and keeps
/// the locale-independent `.` that ffmpeg requires.
fn fmt_num(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(o: &CleanupOptions) -> String {
        o.processing_filters().join(",")
    }

    #[test]
    fn disabled_produces_no_filters() {
        let o = CleanupOptions::default();
        assert!(o.processing_filters().is_empty());
        assert!(o.output_format_filters().is_empty());
        assert!(!o.needs_measurement());
        assert!(!o.needs_intermediate());
    }

    #[test]
    fn downsample_comes_after_the_neural_denoiser() {
        // The whole point: arnndn is locked to 48 kHz, so a 16 kHz resample placed before it
        // gets silently undone by an auto-inserted upsample.
        let o = CleanupOptions::transcription();
        let processing = joined(&o);
        let output = o.output_format_filters().join(",");

        assert!(processing.contains("arnndn"));
        assert!(
            !processing.contains("aresample=16000"),
            "the 16 kHz downsample must not be in the processing chain: {processing}"
        );
        assert!(
            output.contains("aresample=16000"),
            "expected the downsample in the output stage: {output}"
        );
    }

    #[test]
    fn mono_downmix_precedes_the_denoiser() {
        // arnndn works per channel, so downmixing first halves the denoise cost and the
        // intermediate file size.
        let f = joined(&CleanupOptions::transcription());
        let mono_at = f.find("aformat=channel_layouts=mono").expect(&f);
        let denoise_at = f.find("arnndn").expect(&f);
        assert!(mono_at < denoise_at, "{f}");
    }

    #[test]
    fn hum_notches_use_a_high_q() {
        let o = CleanupOptions {
            enabled: true,
            hum_hz: 60,
            ..Default::default()
        };
        let f = joined(&o);
        // Q=1 would be ~60 Hz wide and would gut voice fundamentals.
        assert!(f.contains("equalizer=f=60:t=q:w=14:g=-24"), "{f}");
        assert!(f.contains("equalizer=f=120:t=q:w=14:g=-18"), "{f}");
        assert!(f.contains("equalizer=f=180:t=q:w=14:g=-12"), "{f}");
        assert!(!f.contains(":w=1:"), "notch Q must not fall back to 1: {f}");
    }

    #[test]
    fn hum_fundamental_is_selectable() {
        let o = CleanupOptions {
            enabled: true,
            hum_hz: 50,
            ..Default::default()
        };
        let f = joined(&o);
        assert!(f.contains("equalizer=f=50:"), "{f}");
        assert!(f.contains("equalizer=f=100:"), "{f}");
        assert!(f.contains("equalizer=f=150:"), "{f}");
    }

    #[test]
    fn speechnorm_always_pins_compression() {
        // Omitting `c` inherits a default of 2, i.e. 2x downward compression nobody asked
        // for, which squashes the speaker-level differences diarization uses.
        for o in [
            CleanupOptions::transcription(),
            CleanupOptions::podcast(),
        ] {
            let f = joined(&o);
            assert!(f.contains("speechnorm="), "{f}");
            assert!(
                f.contains(":c="),
                "speechnorm must set compression explicitly: {f}"
            );
        }
        assert!(joined(&CleanupOptions::transcription()).contains(":c=1:"));
    }

    #[test]
    fn gating_and_silence_trimming_never_appear() {
        // `silenceremove` deletes samples, so every timestamp after the first cut is wrong —
        // disqualifying for a transcription target. `agate` inserts transients that VAD and
        // speaker-change detection misread as boundaries.
        for o in [
            CleanupOptions::transcription(),
            CleanupOptions::podcast(),
            CleanupOptions::light_touch(),
        ] {
            let all = format!("{},{}", joined(&o), o.output_format_filters().join(","));
            assert!(!all.contains("silenceremove"), "{all}");
            assert!(!all.contains("agate"), "{all}");
        }
    }

    #[test]
    fn repair_filters_are_opt_in() {
        // adeclick costs ~68% extra wall time for a defect class these inputs don't have.
        for o in [
            CleanupOptions::transcription(),
            CleanupOptions::podcast(),
            CleanupOptions::light_touch(),
        ] {
            assert!(!o.declick);
            assert!(!o.declip);
            assert!(!joined(&o).contains("adeclick"));
        }
    }

    #[test]
    fn light_touch_avoids_the_temp_file() {
        let o = CleanupOptions::light_touch();
        assert!(o.needs_measurement());
        assert!(
            !o.needs_intermediate(),
            "no denoiser means the chain is cheap enough to run twice"
        );
    }

    #[test]
    fn denoising_presets_use_an_intermediate() {
        assert!(CleanupOptions::transcription().needs_intermediate());
        assert!(CleanupOptions::podcast().needs_intermediate());
    }

    #[test]
    fn model_is_referenced_by_bare_filename() {
        // An absolute path can't be used: ':' and '\' are filtergraph metacharacters, so a
        // Windows path like C:\... fails to parse.
        let f = joined(&CleanupOptions::transcription());
        assert!(f.contains("arnndn=m=sh.rnnn:"), "{f}");
        assert!(!f.contains('/'), "model path must stay relative: {f}");
        assert!(!f.contains('\\'), "model path must stay relative: {f}");
    }

    #[test]
    fn static_gain_respects_the_peak_ceiling() {
        let o = CleanupOptions::transcription(); // -19 LUFS, -3 dBTP
        // Loudness alone wants +11 dB, but only 4 dB of peak headroom exists, so the peak
        // budget governs and nothing is pushed into the limiter.
        let m = LoudnormStats {
            input_i: -30.0,
            input_tp: -7.0,
            input_lra: 5.0,
            input_thresh: -40.0,
        };
        assert!((o.static_gain_db(&m) - 4.0).abs() < 1e-9);

        // With ample headroom the loudness target governs.
        let m2 = LoudnormStats {
            input_i: -30.0,
            input_tp: -25.0,
            ..m
        };
        assert!((o.static_gain_db(&m2) - 11.0).abs() < 1e-9);
    }

    #[test]
    fn static_loudness_emits_gain_and_limiter() {
        let o = CleanupOptions::transcription();
        let m = LoudnormStats {
            input_i: -30.0,
            input_tp: -25.0,
            input_lra: 5.0,
            input_thresh: -40.0,
        };
        let f = o.loudness_filters(&m).join(",");
        assert!(f.contains("volume=11dB"), "{f}");
        assert!(!f.contains("loudnorm"), "static mode must not use loudnorm: {f}");
        // The limiter is a separate stage so it can be placed after the resample.
        assert!(!f.contains("alimiter"), "{f}");
        assert_eq!(
            o.limiter_filters(),
            vec!["alimiter=limit=0.7079:level=false".to_string()]
        );
    }

    #[test]
    fn resample_is_inside_the_measured_segment() {
        // Loudness must be measured at the output rate: downsampling removes energy above
        // the new Nyquist limit and shifts the reading.
        let o = CleanupOptions::transcription();
        let pre = o.pre_loudness_filters().join(",");
        let denoise_at = pre.find("arnndn").expect(&pre);
        let resample_at = pre.find("aresample=16000").expect(&pre);
        assert!(
            resample_at > denoise_at,
            "resample must still follow the denoiser: {pre}"
        );
    }

    #[test]
    fn limiter_is_the_very_last_stage() {
        // Resampling and lossy encoding both overshoot, so the limiter must come after
        // everything else to actually bound the file's peaks.
        let o = CleanupOptions::transcription();
        let m = LoudnormStats { input_i: -30.0, input_tp: -25.0, input_lra: 5.0, input_thresh: -40.0 };
        let mut all = o.pre_loudness_filters();
        all.extend(o.loudness_filters(&m));
        all.extend(o.limiter_filters());
        let joined = all.join(",");
        assert!(joined.ends_with("level=false"), "{joined}");
        assert!(joined.find("alimiter").unwrap() > joined.find("aresample=16000").unwrap());
    }

    #[test]
    fn limiter_disables_auto_levelling() {
        // The default (level=true) normalizes up to the ceiling, which would discard the
        // computed gain and clip the output.
        let f = CleanupOptions::transcription().limiter_filters().join(",");
        assert!(f.contains("level=false"), "{f}");
    }

    #[test]
    fn ebu_mode_needs_no_extra_limiter() {
        // loudnorm enforces its own true-peak target.
        assert!(CleanupOptions::podcast().limiter_filters().is_empty());
    }

    #[test]
    fn ebu_loudness_passes_measured_values_through() {
        let o = CleanupOptions::podcast();
        let m = LoudnormStats {
            input_i: -24.3,
            input_tp: -9.0,
            input_lra: 7.5,
            input_thresh: -34.3,
        };
        let f = o.loudness_filters(&m).join(",");
        assert!(f.contains("measured_I=-24.3"), "{f}");
        assert!(f.contains("measured_TP=-9"), "{f}");
        assert!(f.contains("measured_LRA=7.5"), "{f}");
        assert!(f.contains("linear=true"), "{f}");
        assert!(f.contains("LRA=20"), "target LRA must stay wide enough to stay linear: {f}");
    }

    #[test]
    fn number_formatting_is_filtergraph_safe() {
        assert_eq!(fmt_num(6.0), "6");
        assert_eq!(fmt_num(0.85), "0.85");
        assert_eq!(fmt_num(-19.0), "-19");
        assert_eq!(fmt_num(-1.5), "-1.5");
        assert_eq!(fmt_num(0.0), "0");
    }
}

