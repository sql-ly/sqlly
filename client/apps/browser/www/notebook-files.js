// Notebook files for the SQLLY browser build.
//
// The app keeps unsaved notebooks' recovery copies, notebook trust, run
// history and the notebooks saved in this browser in one `notebooks` folder
// of the origin's storage: the Origin Private File System where the page can
// write files (FileSystemFileHandle.createWritable), IndexedDB where it
// cannot, and memory (gone with the tab) when the browser refuses both.
//
// The whole folder is read into memory before the app starts, so the app's
// reads are synchronous like the desktop app's file reads. A write updates
// that copy at once and is persisted in the background, one write at a time,
// keeping only the latest pending text per file. Other tabs of the app hear
// about each write on a BroadcastChannel and update their copy, so a notebook
// saved in one tab shows up as changed in another.
//
// It also opens notebooks from the user's computer (showOpenFilePicker, or a
// file input where the browser has no picker) and saves through the file
// handle a picker returned. The app names handles by an opaque id.
//
// Classic script, loaded before boot.js; boot.js waits for
// `globalThis.sqllyNotebookFilesReady` before launching the app. See
// crates/sqlly-services/src/browser/notebook_files.rs.

(function (global) {
  'use strict';

  const FOLDER = 'notebooks';
  const IDB_NAME = 'sqlly-notebooks';
  const IDB_STORE = 'files';
  const CHANNEL = 'sqlly.notebook-files.v1';
  const MAX_NAME_BYTES = 512;
  // Largest text kept per file (UTF-16 code units).
  const MAX_FILE_CHARS = 32 * 1024 * 1024;
  // Largest notebook file opened from the computer, matching the app's
  // notebook file scan limit.
  const MAX_OPEN_MB = 32;
  const ACCEPT = ['.sqllynb', '.ipynb', '.json'];

  function errorText(error) {
    return String((error && error.message) || error);
  }

  function coded(code, message, extra = {}) {
    const error = new Error(message);
    error.code = code;
    Object.assign(error, extra);
    return error;
  }

  // Same rule as `text_store::valid_name` on the Rust side.
  function validName(name) {
    if (typeof name !== 'string' || !name || new TextEncoder().encode(name).length > MAX_NAME_BYTES) {
      return false;
    }
    return name.split('/').every(
      (segment) =>
        segment.length > 0 &&
        segment !== '.' &&
        segment !== '..' &&
        segment.length <= 255 &&
        !/[\\\u0000-\u001f\u007f-\u009f]/.test(segment),
    );
  }

  function requireName(name) {
    if (!validName(name)) {
      throw new Error(`invalid storage name ${JSON.stringify(name)}`);
    }
    return name;
  }

  // ---------------------------------------------------------------------
  // Backends: kind, loadAll() -> [[name, text, modified]], write, remove.

  async function opfsBackend(storage) {
    if (!storage || typeof storage.getDirectory !== 'function') {
      return null;
    }
    const root = await (await storage.getDirectory()).getDirectoryHandle(FOLDER, { create: true });
    // Safari before 26 has OPFS for workers only: no createWritable.
    const probe = await root.getFileHandle('.sqlly-probe', { create: true });
    const writable = typeof probe.createWritable === 'function';
    try {
      await root.removeEntry('.sqlly-probe');
    } catch {}
    if (!writable) {
      return null;
    }

    async function folderOf(name, create) {
      const parts = name.split('/');
      let dir = root;
      for (const part of parts.slice(0, -1)) {
        dir = await dir.getDirectoryHandle(part, { create });
      }
      return { dir, leaf: parts[parts.length - 1] };
    }

    async function walk(dir, prefix, out) {
      for await (const [name, handle] of dir.entries()) {
        const path = prefix ? `${prefix}/${name}` : name;
        if (handle.kind === 'directory') {
          await walk(handle, path, out);
        } else if (validName(path) && !name.startsWith('.sqlly-')) {
          const file = await handle.getFile();
          out.push([path, await file.text(), file.lastModified]);
        }
      }
      return out;
    }

    return {
      kind: 'opfs',
      loadAll: () => walk(root, '', []),
      async write(name, text) {
        const { dir, leaf } = await folderOf(name, true);
        const handle = await dir.getFileHandle(leaf, { create: true });
        const stream = await handle.createWritable();
        try {
          await stream.write(text);
          await stream.close();
        } catch (error) {
          try {
            await stream.abort();
          } catch {}
          throw error;
        }
      },
      async remove(name) {
        let place;
        try {
          place = await folderOf(name, false);
        } catch {
          return;
        }
        try {
          await place.dir.removeEntry(place.leaf);
        } catch (error) {
          if (error && error.name !== 'NotFoundError') throw error;
        }
      },
    };
  }

  function request(req) {
    return new Promise((resolve, reject) => {
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  }

  async function indexedDbBackend(indexedDB) {
    if (!indexedDB || typeof indexedDB.open !== 'function') {
      return null;
    }
    const open = indexedDB.open(IDB_NAME, 1);
    open.onupgradeneeded = () => open.result.createObjectStore(IDB_STORE);
    const db = await request(open);
    const store = (mode) => db.transaction(IDB_STORE, mode).objectStore(IDB_STORE);
    return {
      kind: 'indexeddb',
      async loadAll() {
        const files = store('readonly');
        const [keys, values] = await Promise.all([request(files.getAllKeys()), request(files.getAll())]);
        return keys
          .map((name, i) => [name, values[i] && values[i].text, values[i] && values[i].modified])
          .filter(([name, text]) => validName(name) && typeof text === 'string');
      },
      write: (name, text) => request(store('readwrite').put({ text, modified: Date.now() }, name)),
      remove: (name) => request(store('readwrite').delete(name)),
    };
  }

  function memoryBackend() {
    return {
      kind: 'memory',
      loadAll: async () => [],
      write: async () => {},
      remove: async () => {},
    };
  }

  // ---------------------------------------------------------------------
  // The in-memory copy and its write-behind queue.

  function createStore(backend, entries, { now, channel }) {
    const files = new Map();
    for (const [name, text, modified] of entries) {
      files.set(name, { text, modified: Number(modified) || 0 });
    }
    const pending = new Map();
    let running = null;
    let lastError = null;

    function kick() {
      if (running) return;
      running = (async () => {
        while (pending.size > 0) {
          const [name, op] = pending.entries().next().value;
          pending.delete(name);
          try {
            if (op.text === null) {
              await backend.remove(name);
            } else {
              await backend.write(name, op.text);
            }
            lastError = null;
          } catch (error) {
            // Usually QuotaExceededError. The copy in memory stays current.
            lastError = errorText(error);
            console.warn(`notebook storage: could not save ${name}`, error);
          }
        }
        running = null;
      })();
    }

    function stamp() {
      // Strictly increasing, so two writes in one millisecond still differ.
      const at = now();
      stamp.last = Math.max(at, (stamp.last || 0) + 1);
      return stamp.last;
    }

    function put(name, text, announce) {
      if (text === null) {
        files.delete(name);
      } else {
        files.set(name, { text, modified: stamp() });
      }
      pending.set(name, { text });
      kick();
      if (announce && channel) {
        const entry = files.get(name);
        try {
          channel.postMessage({ name, text, modified: entry ? entry.modified : null });
        } catch {}
      }
    }

    if (channel) {
      channel.onmessage = ({ data }) => {
        if (!data || !validName(data.name)) return;
        if (data.text === null) {
          files.delete(data.name);
        } else if (typeof data.text === 'string') {
          files.set(data.name, { text: data.text, modified: Number(data.modified) || stamp() });
        }
      };
    }

    return {
      backend: () => backend.kind,
      read(name) {
        const entry = files.get(requireName(name));
        return entry ? entry.text : null;
      },
      modified(name) {
        const entry = files.get(requireName(name));
        return entry ? entry.modified : null;
      },
      write(name, text) {
        requireName(name);
        text = String(text);
        if (text.length > MAX_FILE_CHARS) {
          throw coded('too_large', `${name} is larger than browser storage allows`);
        }
        put(name, text, true);
      },
      remove(name) {
        requireName(name);
        if (files.has(name)) put(name, null, true);
      },
      list(prefix = '') {
        return [...files.keys()].filter((name) => name.startsWith(String(prefix))).sort();
      },
      lastError: () => lastError,
      flush: () => running || Promise.resolve(),
    };
  }

  // ---------------------------------------------------------------------
  // Files on the user's computer.

  function createFiles({ window: win, document: doc }) {
    const handles = new Map();
    let nextHandle = 1;

    function keep(handle) {
      const id = `h${nextHandle++}`;
      handles.set(id, handle);
      return id;
    }

    function types(description) {
      return [{ description: description || 'SQLLY notebook', accept: { 'application/json': ACCEPT } }];
    }

    async function readPicked(file) {
      if (file.size > MAX_OPEN_MB * 1024 * 1024) {
        throw coded('too_large', `${file.name} is larger than ${MAX_OPEN_MB} MB`, { limitMb: MAX_OPEN_MB });
      }
      return file.text();
    }

    function pickWithInput() {
      return new Promise((resolve, reject) => {
        const input = doc.createElement('input');
        input.type = 'file';
        input.accept = ACCEPT.join(',');
        input.style.display = 'none';
        let settled = false;
        const finish = (fn, value) => {
          if (settled) return;
          settled = true;
          input.remove();
          fn(value);
        };
        input.addEventListener('change', () => {
          const file = input.files && input.files[0];
          if (!file) {
            finish(resolve, null);
            return;
          }
          readPicked(file).then(
            (text) => finish(resolve, { name: file.name, text, handle: null }),
            (error) => finish(reject, error),
          );
        });
        input.addEventListener('cancel', () => finish(resolve, null));
        doc.body.append(input);
        input.click();
      });
    }

    async function permitted(handle, ask) {
      if (typeof handle.queryPermission !== 'function') return true;
      if ((await handle.queryPermission({ mode: 'readwrite' })) === 'granted') return true;
      if (!ask || typeof handle.requestPermission !== 'function') return false;
      return (await handle.requestPermission({ mode: 'readwrite' })) === 'granted';
    }

    async function writeHandle(handle, text) {
      const stream = await handle.createWritable();
      await stream.write(text);
      await stream.close();
    }

    const aborted = (error) => error && error.name === 'AbortError';

    return {
      canSaveFiles: () => typeof win.showSaveFilePicker === 'function',
      async pickFile(description) {
        if (typeof win.showOpenFilePicker !== 'function') {
          return pickWithInput();
        }
        let picked;
        try {
          [picked] = await win.showOpenFilePicker({ multiple: false, types: types(description) });
        } catch (error) {
          if (aborted(error)) return null;
          throw error;
        }
        const file = await picked.getFile();
        const text = await readPicked(file);
        return { name: file.name, text, handle: keep(picked) };
      },
      // Only chooses the file: the app writes it with saveFile once it knows
      // the name, which decides the format (.ipynb or .sqllynb).
      async chooseSaveFile(suggestedName, description) {
        if (typeof win.showSaveFilePicker !== 'function') {
          throw coded('unsupported', 'this browser cannot save to a chosen file');
        }
        let handle;
        try {
          handle = await win.showSaveFilePicker({ suggestedName, types: types(description) });
        } catch (error) {
          if (aborted(error)) return null;
          throw error;
        }
        return { name: handle.name || suggestedName, handle: keep(handle) };
      },
      async saveFile(id, text, ask) {
        const handle = handles.get(id);
        if (!handle) {
          throw coded('no_handle', 'the file is no longer open in this page');
        }
        if (!(await permitted(handle, ask))) {
          throw coded('permission', 'the browser did not allow writing to the file');
        }
        await writeHandle(handle, String(text));
      },
    };
  }

  // ---------------------------------------------------------------------

  async function install(env = {}) {
    const now = env.now || (() => Date.now());
    let backend = null;
    for (const open of [() => opfsBackend(env.storage), () => indexedDbBackend(env.indexedDB)]) {
      try {
        backend = await open();
      } catch (error) {
        console.warn('notebook storage backend unavailable', error);
        backend = null;
      }
      if (!backend) continue;
      try {
        const entries = await backend.loadAll();
        let channel = null;
        if (typeof env.BroadcastChannel === 'function') {
          try {
            channel = new env.BroadcastChannel(CHANNEL);
          } catch {}
        }
        const store = createStore(backend, entries, { now, channel });
        return Object.freeze({ ...store, ...createFiles(env) });
      } catch (error) {
        console.warn(`notebook storage (${backend.kind}) could not be read`, error);
      }
    }
    return Object.freeze({ ...createStore(memoryBackend(), [], { now, channel: null }), ...createFiles(env) });
  }

  global.SqllyNotebookFiles = Object.freeze({ install, validName });

  if (typeof global.navigator !== 'undefined' && typeof global.document !== 'undefined') {
    global.sqllyNotebookFilesReady = install({
      storage: global.navigator.storage,
      indexedDB: global.indexedDB,
      BroadcastChannel: global.BroadcastChannel,
      window: global,
      document: global.document,
    }).then((api) => {
      global.sqllyNotebookFiles = api;
      return api;
    });
  }
})(globalThis);
