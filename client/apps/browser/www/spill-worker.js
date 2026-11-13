// Spill worker: keeps large query results in the Origin Private File System
// so the page holds only the rows on screen.
//
// The app encodes rows itself (sqlly-core's spill format: a data file of row
// records and an `.idx` file of row offsets) and sends finished byte runs
// here. This worker only appends them and reads byte ranges back, through
// OPFS sync access handles, which exist only in dedicated workers.
//
// Messages run one at a time, in arrival order, so a read always sees every
// append the page sent before it. See crates/sqlly-services/src/browser/spill.rs.

const DIR = 'sqlly-spill';
const HEADER = 8; // magic + version at the start of both files

let dirPromise;
const spills = new Map(); // name -> { data, index, failed }

function folder() {
  dirPromise ??= navigator.storage
    .getDirectory()
    .then((root) => root.getDirectoryHandle(DIR, { create: true }));
  return dirPromise;
}

function supported() {
  return (
    typeof navigator !== 'undefined' &&
    !!navigator.storage &&
    typeof navigator.storage.getDirectory === 'function' &&
    typeof FileSystemFileHandle !== 'undefined' &&
    typeof FileSystemFileHandle.prototype.createSyncAccessHandle === 'function'
  );
}

async function create(name) {
  const entry = { data: null, index: null, failed: null };
  spills.set(name, entry);
  try {
    const dir = await folder();
    for (const [key, file] of [
      ['data', name],
      ['index', `${name}.idx`],
    ]) {
      const handle = await dir.getFileHandle(file, { create: true });
      entry[key] = await handle.createSyncAccessHandle();
      entry[key].truncate(0);
    }
  } catch (error) {
    entry.failed = String(error?.message ?? error);
  }
}

function spill(name) {
  const entry = spills.get(name);
  if (!entry) throw new Error(`no spill named ${name}`);
  if (entry.failed) throw new Error(entry.failed);
  return entry;
}

function append(name, data, index) {
  const entry = spills.get(name);
  if (!entry || entry.failed) return;
  try {
    for (const [handle, bytes] of [
      [entry.data, new Uint8Array(data)],
      [entry.index, new Uint8Array(index)],
    ]) {
      let at = handle.getSize();
      let rest = bytes;
      while (rest.length > 0) {
        const wrote = handle.write(rest, { at });
        if (wrote <= 0) throw new Error('browser storage stopped accepting writes');
        at += wrote;
        rest = rest.subarray(wrote);
      }
    }
  } catch (error) {
    // Usually QuotaExceededError. Reads of this spill report it from now on.
    entry.failed = String(error?.message ?? error);
  }
}

function readAt(handle, at, length) {
  const out = new Uint8Array(length);
  let done = 0;
  while (done < length) {
    const read = handle.read(out.subarray(done), { at: at + done });
    if (read <= 0) throw new Error('spill file ended early');
    done += read;
  }
  return out;
}

function offsetAt(entry, row) {
  const bytes = readAt(entry.index, HEADER + row * 8, 8);
  return Number(new DataView(bytes.buffer).getBigUint64(0, true));
}

// The data-file bytes of rows [start, end): one contiguous run, because rows
// are appended in order. Clamped to the rows written so far.
function page(name, start, end) {
  const entry = spill(name);
  const rows = Math.max(0, (entry.index.getSize() - HEADER) / 8);
  end = Math.min(end, rows);
  if (start >= end) return new ArrayBuffer(0);
  const from = offsetAt(entry, start);
  const to = end < rows ? offsetAt(entry, end) : entry.data.getSize();
  return readAt(entry.data, from, to - from).buffer;
}

// Every row record in the data file, for a full scan on the page.
function body(name) {
  const entry = spill(name);
  const size = entry.data.getSize();
  return readAt(entry.data, HEADER, Math.max(0, size - HEADER)).buffer;
}

async function remove(name) {
  const entry = spills.get(name);
  spills.delete(name);
  for (const handle of [entry?.data, entry?.index]) {
    try {
      handle?.close();
    } catch {}
  }
  const dir = await folder();
  for (const file of [name, `${name}.idx`]) {
    try {
      await dir.removeEntry(file);
    } catch {}
  }
}

// Spill files left by tabs that closed or crashed. Another open tab's files
// stay locked by its own handles, so removing them fails and they are kept.
async function sweep() {
  const dir = await folder();
  const names = [];
  for await (const name of dir.keys()) names.push(name);
  let removed = 0;
  for (const name of names) {
    const base = name.endsWith('.idx') ? name.slice(0, -4) : name;
    if (spills.has(base)) continue;
    try {
      await dir.removeEntry(name);
      removed += 1;
    } catch {}
  }
  return removed;
}

async function estimate() {
  const { quota = 0, usage = 0 } = await navigator.storage.estimate();
  return { quota, usage };
}

async function handle({ op, args }) {
  switch (op) {
    case 'supported':
      return [supported()];
    case 'create':
      return [await create(args.name)];
    case 'append':
      return [append(args.name, args.data, args.index)];
    case 'page': {
      const bytes = page(args.name, args.start, args.end);
      return [bytes, [bytes]];
    }
    case 'body': {
      const bytes = body(args.name);
      return [bytes, [bytes]];
    }
    case 'remove':
      return [await remove(args.name)];
    case 'sweep':
      return [await sweep()];
    case 'estimate':
      return [await estimate()];
    default:
      throw new Error(`unknown spill op ${op}`);
  }
}

let queue = Promise.resolve();
self.onmessage = ({ data }) => {
  queue = queue.then(async () => {
    try {
      const [value, transfer = []] = await handle(data);
      if (data.id) self.postMessage({ id: data.id, ok: true, value }, transfer);
    } catch (error) {
      if (data.id) self.postMessage({ id: data.id, ok: false, error: String(error?.message ?? error) });
    }
  });
};
