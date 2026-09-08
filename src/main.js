const { invoke } = window.__TAURI__.core;
const { open } = window.__TAURI__.dialog;
const { getCurrentWebview } = window.__TAURI__.webview;
const { listen } = window.__TAURI__.event;

const dropzone = document.getElementById("dropzone");
const browseBtn = document.getElementById("browseBtn");
const convertBtn = document.getElementById("convertBtn");
const clearBtn = document.getElementById("clearBtn");
const formatSelect = document.getElementById("formatSelect");
const presetSelect = document.getElementById("presetSelect");
const presetNote = document.getElementById("presetNote");
const advanced = document.getElementById("advanced");
const filelist = document.getElementById("filelist");

const AUDIO_EXTENSIONS = [
  "mp3", "wav", "flac", "ogg", "m4a", "aac", "wma", "opus", "aiff", "aif",
];

const PRESET_NOTES = {
  transcription:
    "Neural denoise, quiet speakers levelled, one fixed gain, 16 kHz mono — what Whisper and pyannote want.",
  podcast: "Neural denoise, gentle compression, EBU R128 at −16 LUFS, 48 kHz stereo.",
  light: "High-pass and loudness only. For recordings that are already clean.",
  none: "No processing — just a format change.",
};

// Advanced-panel controls, keyed by the CleanupOptions field they drive.
const FIELDS = {
  declick: { el: "optDeclick", kind: "bool" },
  declip: { el: "optDeclip", kind: "bool" },
  highpassHz: { el: "optHighpass", kind: "int", out: "optHighpassOut", fmt: (v) => (v > 0 ? `${v} Hz` : "off") },
  humHz: { el: "optHum", kind: "int" },
  denoise: { el: "optDenoise", kind: "str" },
  denoiseMix: { el: "optMix", kind: "float", out: "optMixOut", fmt: (v) => `${Math.round(v * 100)}%` },
  speechnorm: { el: "optSpeechnorm", kind: "bool" },
  compressor: { el: "optCompressor", kind: "bool" },
  loudness: { el: "optLoudness", kind: "str" },
  targetLufs: { el: "optLufs", kind: "float", out: "optLufsOut", fmt: (v) => `${v} LUFS` },
  outputRate: { el: "optRate", kind: "int" },
  mono: { el: "optMono", kind: "bool" },
};

// Fields the backend owns but the UI doesn't expose; carried through untouched.
let hiddenOptions = {};
let running = false;

// Map of path -> { row, state, els }
const queue = new Map();

function extOf(path) {
  const clean = path.split(/[?#]/)[0];
  const parts = clean.split(".");
  return parts.length > 1 ? parts.pop().toLowerCase() : "";
}

function baseName(path) {
  return path.split(/[\\/]/).pop();
}

function el(id) {
  return document.getElementById(id);
}

// --- Cleanup options ---------------------------------------------------------------

function readOptions() {
  const opts = { ...hiddenOptions };
  for (const [key, spec] of Object.entries(FIELDS)) {
    const node = el(spec.el);
    if (spec.kind === "bool") opts[key] = node.checked;
    else if (spec.kind === "int") opts[key] = parseInt(node.value, 10) || 0;
    else if (spec.kind === "float") opts[key] = parseFloat(node.value);
    else opts[key] = node.value;
  }
  // Anything other than "no processing at all" counts as enabled.
  opts.enabled =
    presetSelect.value !== "none" &&
    (opts.declick ||
      opts.declip ||
      opts.highpassHz > 0 ||
      opts.humHz > 0 ||
      opts.denoise !== "off" ||
      opts.speechnorm ||
      opts.compressor ||
      opts.loudness !== "off" ||
      opts.outputRate > 0 ||
      opts.mono);
  return opts;
}

function writeOptions(opts) {
  hiddenOptions = { ...opts };
  for (const [key, spec] of Object.entries(FIELDS)) {
    const node = el(spec.el);
    if (spec.kind === "bool") node.checked = Boolean(opts[key]);
    else node.value = String(opts[key]);
    delete hiddenOptions[key];
  }
  syncReadouts();
}

function syncReadouts() {
  for (const spec of Object.values(FIELDS)) {
    if (!spec.out) continue;
    const node = el(spec.el);
    const value = spec.kind === "int" ? parseInt(node.value, 10) : parseFloat(node.value);
    el(spec.out).textContent = spec.fmt(value);
  }
  // The room-tone slider only means anything for the neural denoiser.
  const mixField = el("optMix").closest(".field");
  mixField.classList.toggle("is-muted", el("optDenoise").value !== "neural");
  const lufsField = el("optLufs").closest(".field");
  lufsField.classList.toggle("is-muted", el("optLoudness").value === "off");
}

async function loadPreset(name) {
  try {
    const opts = await invoke("preset_options", { name });
    writeOptions(opts);
  } catch (err) {
    console.error(`Could not load preset ${name}:`, err);
  }
  presetNote.textContent = PRESET_NOTES[name] ?? "";
  advanced.classList.toggle("is-disabled", name === "none");
}

presetSelect.addEventListener("change", () => loadPreset(presetSelect.value));

for (const spec of Object.values(FIELDS)) {
  el(spec.el).addEventListener("input", syncReadouts);
}

// --- Queue rows --------------------------------------------------------------------

function updateActionButtons() {
  const hasFiles = queue.size > 0;
  clearBtn.disabled = !hasFiles || running;
  convertBtn.disabled = !hasFiles || running;
  presetSelect.disabled = running;
  formatSelect.disabled = running;
}

function addFiles(paths) {
  let added = 0;
  for (const path of paths) {
    if (queue.has(path)) continue;
    if (!AUDIO_EXTENSIONS.includes(extOf(path))) continue;

    const row = document.createElement("div");
    row.className = "file-row state-queued";

    const name = document.createElement("div");
    name.className = "file-row__name";
    name.title = path;
    name.textContent = baseName(path);

    const meter = document.createElement("div");
    meter.className = "file-row__meter";
    const fill = document.createElement("div");
    fill.className = "file-row__meter-fill";
    meter.appendChild(fill);

    const status = document.createElement("div");
    status.className = "file-row__status";
    status.textContent = "Queued";

    const cancel = document.createElement("button");
    cancel.className = "file-row__cancel";
    cancel.type = "button";
    cancel.textContent = "Stop";
    cancel.title = "Stop processing this file";
    cancel.addEventListener("click", () => {
      cancel.disabled = true;
      invoke("cancel_job", { jobId: path }).catch((err) =>
        console.error(`Could not cancel ${path}:`, err)
      );
    });

    const detail = document.createElement("div");
    detail.className = "file-row__detail";

    row.append(name, meter, status, cancel, detail);
    filelist.appendChild(row);
    queue.set(path, { row, state: "queued", els: { fill, status, cancel, detail } });
    added++;
  }
  if (added > 0) updateActionButtons();
  return added;
}

function setRowState(path, state, statusText) {
  const entry = queue.get(path);
  if (!entry) return;
  entry.state = state;
  entry.row.className = `file-row state-${state}`;
  entry.els.status.textContent = statusText;
  entry.els.cancel.disabled = state !== "converting";
  if (state === "queued") entry.els.fill.style.width = "0%";
  if (state === "done" || state === "error" || state === "cancelled") {
    entry.els.fill.style.width = "100%";
  }
}

function setRowDetail(path, text) {
  const entry = queue.get(path);
  if (entry) entry.els.detail.textContent = text ?? "";
}

function formatEta(seconds) {
  if (!Number.isFinite(seconds) || seconds < 1) return "";
  if (seconds < 60) return `${Math.round(seconds)}s left`;
  const mins = Math.floor(seconds / 60);
  const secs = Math.round(seconds % 60);
  return mins < 60 ? `${mins}m ${secs}s left` : `${Math.floor(mins / 60)}h ${mins % 60}m left`;
}

// --- Real progress from the backend ------------------------------------------------

listen("cleanup://progress", (event) => {
  const { inputPath, stage, fraction, etaSeconds } = event.payload;
  const entry = queue.get(inputPath);
  if (!entry || entry.state !== "converting") return;

  if (fraction === null || fraction === undefined) {
    // Duration unknown, so a percentage would be a lie — show motion instead.
    entry.row.classList.add("is-indeterminate");
    entry.els.status.textContent = `${stage}…`;
    return;
  }
  entry.row.classList.remove("is-indeterminate");
  entry.els.fill.style.width = `${Math.round(fraction * 100)}%`;
  const eta = formatEta(etaSeconds);
  entry.els.status.textContent = `${stage} ${Math.round(fraction * 100)}%`;
  entry.els.detail.textContent = eta;
});

// --- Drag & drop (native OS file drop) ---------------------------------------------

getCurrentWebview().onDragDropEvent((event) => {
  const type = event.payload.type;
  if (type === "over") {
    dropzone.classList.add("is-active");
  } else if (type === "drop") {
    dropzone.classList.remove("is-active");
    addFiles(event.payload.paths);
  } else {
    dropzone.classList.remove("is-active");
  }
});

browseBtn.addEventListener("click", async () => {
  const selected = await open({
    multiple: true,
    filters: [{ name: "Audio", extensions: AUDIO_EXTENSIONS }],
  });
  if (!selected) return;
  addFiles(Array.isArray(selected) ? selected : [selected]);
});

clearBtn.addEventListener("click", () => {
  queue.clear();
  filelist.innerHTML = "";
  updateActionButtons();
});

// --- Convert -----------------------------------------------------------------------

convertBtn.addEventListener("click", async () => {
  const target = formatSelect.value;
  const cleanup = readOptions();
  const pending = [...queue.entries()].filter(([, v]) => v.state !== "done");

  running = true;
  updateActionButtons();

  // Deliberately sequential: the neural denoiser is CPU-bound, and running several
  // multi-hour jobs at once would thrash the machine and multiply temp-file usage.
  for (const [path] of pending) {
    setRowState(path, "converting", "Starting…");
    setRowDetail(path, "");
    try {
      const result = await invoke("process_audio_file", {
        inputPath: path,
        targetFormat: target,
        cleanup,
      });
      if (result.cancelled) {
        setRowState(path, "cancelled", "Stopped");
        setRowDetail(path, "");
      } else {
        setRowState(path, "done", `Done → ${baseName(result.outputPath)}`);
        setRowDetail(path, describeResult(result));
      }
    } catch (err) {
      const message =
        typeof err === "string" ? err : err?.message ?? JSON.stringify(err ?? "Failed");
      setRowState(path, "error", "Failed");
      setRowDetail(path, message.split("\n")[0]);
      queue.get(path).row.title = message;
      console.error(`Conversion failed for ${path}:`, message);
    }
  }

  running = false;
  updateActionButtons();
});

function describeResult(result) {
  const parts = [];
  if (result.measuredLufs !== null && result.measuredLufs !== undefined) {
    const from = result.measuredLufs.toFixed(1);
    if (result.appliedGainDb !== null && result.appliedGainDb !== undefined) {
      const gain = result.appliedGainDb;
      const to = (result.measuredLufs + gain).toFixed(1);
      parts.push(`${from} → ${to} LUFS (${gain >= 0 ? "+" : ""}${gain.toFixed(1)} dB)`);
    } else if (result.targetLufs !== null && result.targetLufs !== undefined) {
      parts.push(`${from} → ${result.targetLufs.toFixed(1)} LUFS`);
    }
  }
  return parts.join(" · ");
}

loadPreset(presetSelect.value);
