// Hands the missing-models dialog's downloads to the server. The dialog calls the desktop app's
// bridge when one is present and local, so providing it routes both Download and Download all
// to the download node instead of a browser download on the viewing computer.
import { app } from "../../scripts/app.js";
import { api } from "../../scripts/api.js";

const ROUTE = "/dotcore/model-downloads";
const EVENT = "dotcore-model-download";
const PROGRESS_STEP = 10;

// One progress toast per running download, replaced as it advances.
const progressToasts = new Map();
// Downloads whose final event arrived, which can precede the reply that started them.
const finished = new Set();
// Whether a download succeeded since the editor last refreshed its model lists.
let succeeded = false;

// Set as the script loads, before any workflow opens the dialog; a real desktop app keeps its own.
window.__comfyDesktop2 ??= { downloadModel, isRemote: () => false };

app.registerExtension({
  name: "dotcore.modelDownloads",
  setup() {
    api.addEventListener(EVENT, ({ detail }) => onProgress(detail));
  },
});

async function downloadModel(url, name, directory) {
  finished.delete(`${directory}/${name}`);
  let reply;
  try {
    const response = await api.fetchApi(ROUTE, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ url, name, directory }),
    });
    const text = await response.text();
    try {
      reply = { ok: response.ok, ...JSON.parse(text) };
    } catch {
      reply = { ok: false, error: `the server answered ${response.status}: ${text}` };
    }
  } catch (error) {
    reply = { ok: false, error: `the server did not answer: ${error}` };
  }

  if (reply.download?.state === "downloading" && !reply.ok) {
    toast({ severity: "info", summary: `${name} is already downloading`, life: 5000 });
    return;
  }
  if (!reply.ok) {
    toast({ severity: "error", summary: `${name} was not downloaded`, detail: reply.error });
    return;
  }
  if (!finished.has(reply.download.id)) showProgress(reply.download);
}

function onProgress(download) {
  if (download.state === "downloading") {
    showProgress(download);
    return;
  }

  finished.add(download.id);
  clearProgress(download.id);
  if (download.state === "failed") {
    toast({
      severity: "error",
      summary: `${download.name} was not downloaded`,
      detail: download.error,
    });
  } else {
    succeeded = true;
    toast({
      severity: "success",
      summary: `${download.name} downloaded`,
      detail: `Saved to the ${download.directory} folder on the server.`,
      life: 5000,
    });
  }

  // Once the last running download ends, reloads the node definitions, which list the model
  // files, and rechecks the workflow, so new models are selectable and leave the missing-models
  // panel without a restart.
  if (progressToasts.size === 0 && succeeded) {
    succeeded = false;
    app.refreshMissingModels?.() ?? app.refreshComboInNodes();
  }
}

function showProgress(download) {
  const percent = download.total ? Math.floor((download.received / download.total) * 100) : null;
  const step = percent === null ? null : percent - (percent % PROGRESS_STEP);
  const previous = progressToasts.get(download.id);
  if (previous && previous.step === step) return;

  const message = {
    severity: "info",
    summary: `Downloading ${download.name} to the server`,
    detail:
      step === null
        ? `${formatSize(download.received)} received`
        : `${step}% of ${formatSize(download.total)}`,
  };
  if (previous) app.extensionManager.toast.remove(previous.message);
  toast(message);
  progressToasts.set(download.id, { step, message });
}

function clearProgress(id) {
  const previous = progressToasts.get(id);
  if (!previous) return;
  app.extensionManager.toast.remove(previous.message);
  progressToasts.delete(id);
}

function toast(message) {
  app.extensionManager.toast.add(message);
}

function formatSize(bytes) {
  return bytes >= 2 ** 30
    ? `${(bytes / 2 ** 30).toFixed(2)} GiB`
    : `${(bytes / 2 ** 20).toFixed(1)} MiB`;
}
