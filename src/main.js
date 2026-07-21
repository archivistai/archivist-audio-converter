const { invoke } = window.__TAURI__.core;
const { open } = window.__TAURI__.dialog;
const { getCurrentWebview } = window.__TAURI__.webview;

const dropzone = document.getElementById("dropzone");
const browseBtn = document.getElementById("browseBtn");
const convertBtn = document.getElementById("convertBtn");
const clearBtn = document.getElementById("clearBtn");
const formatSelect = document.getElementById("formatSelect");
const filelist = document.getElementById("filelist");

const AUDIO_EXTENSIONS = [
  "mp3", "wav", "flac", "ogg", "m4a", "aac", "wma", "opus", "aiff", "aif",
];

// Map of path -> { row, state }
const queue = new Map();

function extOf(path) {
  const clean = path.split(/[?#]/)[0];
  const parts = clean.split(".");
  return parts.length > 1 ? parts.pop().toLowerCase() : "";
}

function baseName(path) {
  return path.split(/[\\/]/).pop();
}

function updateActionButtons() {
  const hasFiles = queue.size > 0;
  clearBtn.disabled = !hasFiles;
  const anyBusy = [...queue.values()].some((f) => f.state === "converting");
  convertBtn.disabled = !hasFiles || anyBusy;
}

function addFiles(paths) {
  let added = 0;
  for (const path of paths) {
    if (queue.has(path)) continue;
    const ext = extOf(path);
    if (!AUDIO_EXTENSIONS.includes(ext)) continue;

    const row = document.createElement("div");
    row.className = "file-row state-queued";
    row.innerHTML = `
      <div class="file-row__name" title="${path}">${baseName(path)}</div>
      <div class="file-row__meter"><div class="file-row__meter-fill"></div></div>
      <div class="file-row__status">Queued</div>
    `;
    filelist.appendChild(row);
    queue.set(path, { row, state: "queued" });
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
  entry.row.querySelector(".file-row__status").textContent = statusText;
  if (state !== "converting") {
    entry.row.querySelector(".file-row__meter-fill").style.width =
      state === "queued" ? "0%" : "100%";
  }
}

// --- Drag & drop (native OS file drop) ---

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

// --- Browse button ---

browseBtn.addEventListener("click", async () => {
  const selected = await open({
    multiple: true,
    filters: [{ name: "Audio", extensions: AUDIO_EXTENSIONS }],
  });
  if (!selected) return;
  const paths = Array.isArray(selected) ? selected : [selected];
  addFiles(paths);
});

// --- Clear ---

clearBtn.addEventListener("click", () => {
  queue.clear();
  filelist.innerHTML = "";
  updateActionButtons();
});

// --- Convert ---

convertBtn.addEventListener("click", async () => {
  const target = formatSelect.value;
  const pending = [...queue.entries()].filter(([, v]) => v.state !== "done");

  convertBtn.disabled = true;
  clearBtn.disabled = true;

  for (const [path] of pending) {
    setRowState(path, "converting", "Converting…");
    try {
      const result = await invoke("convert_audio_file", {
        inputPath: path,
        targetFormat: target,
      });
      setRowState(path, "done", `Done → ${baseName(result.output_path)}`);
    } catch (err) {
      const message =
        typeof err === "string" ? err : err?.message ?? JSON.stringify(err ?? "Failed");
      setRowState(path, "error", "Failed");
      const row = queue.get(path)?.row;
      if (row) row.title = message;
      console.error(`Conversion failed for ${path}:`, message);
    }
  }

  updateActionButtons();
});
