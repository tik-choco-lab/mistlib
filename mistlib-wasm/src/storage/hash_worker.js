// One worker per page, retired when idle. Only private block copies are transferred.
const source = `self.onmessage = async ({ data: { id, bytes } }) => {
  try {
    const digest = await crypto.subtle.digest('SHA-256', bytes);
    self.postMessage({ id, digest }, [digest]);
  } catch (error) {
    self.postMessage({ id, error: String(error) });
  }
};`;
let worker, idleTimer, disabled = false, nextId = 0;
const pending = new Map();

function failWorker(error) {
  disabled = true;
  clearTimeout(idleTimer);
  worker?.terminate();
  worker = undefined;
  for (const job of pending.values()) {
    clearTimeout(job.timer);
    job.reject(error);
  }
  pending.clear();
}

function getWorker() {
  clearTimeout(idleTimer);
  if (worker) return worker;
  const url = URL.createObjectURL(new Blob([source], { type: 'text/javascript' }));
  try { worker = new Worker(url); }
  finally { URL.revokeObjectURL(url); }
  worker.onmessage = ({ data: { id, digest, error } }) => {
    const job = pending.get(id);
    if (!job) return;
    pending.delete(id);
    clearTimeout(job.timer);
    if (error) job.reject(new Error(error));
    else job.resolve(digest);
    if (!pending.size) idleTimer = setTimeout(() => {
      worker?.terminate();
      worker = undefined;
    }, 1000);
  };
  worker.onerror = event => {
    event.preventDefault();
    failWorker(new Error('Storage digest worker failed'));
  };
  worker.onmessageerror = () => failWorker(new Error('Invalid storage digest worker message'));
  return worker;
}

export function hashBlock(bytes) {
  if (disabled || pending.size >= 64) return crypto.subtle.digest('SHA-256', bytes);
  let active;
  try { active = getWorker(); }
  catch {
    disabled = true;
    return crypto.subtle.digest('SHA-256', bytes);
  }
  // The wasm view is used only synchronously. It may detach on memory growth
  // while the worker is busy, so it must never be retained for async fallback.
  const copy = new Uint8Array(bytes);
  return new Promise((resolve, reject) => {
    const id = nextId++;
    const timer = setTimeout(() => failWorker(new Error('Storage digest worker timeout')), 10000);
    pending.set(id, { resolve, reject, timer });
    try { active.postMessage({ id, bytes: copy }, [copy.buffer]); }
    catch (error) { failWorker(error); }
  });
}
