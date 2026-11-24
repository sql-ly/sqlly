// Notebook files (www/notebook-files.js) over in-memory stand-ins for the
// Origin Private File System, IndexedDB, BroadcastChannel and the File System
// Access pickers: the folder is read before launch and answers synchronously,
// writes persist in the background in order (latest text per file), browsers
// without OPFS writes fall back to IndexedDB and then to memory, other tabs
// see each write, and files on the computer open and save through handles.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../www/notebook-files.js'), 'utf8');

function load(extra = {}) {
  const context = { console: { warn() {} }, TextEncoder, ...extra };
  vm.createContext(context);
  vm.runInContext(source, context);
  return context;
}

class FakeWritable {
  constructor(file, quota) {
    this.file = file;
    this.quota = quota;
    this.buffer = '';
  }
  async write(text) {
    if (this.quota !== undefined && text.length > this.quota) {
      const error = new Error('QuotaExceededError');
      error.name = 'QuotaExceededError';
      throw error;
    }
    this.buffer += text;
  }
  async close() {
    this.file.text = this.buffer;
    this.file.lastModified = (this.file.lastModified || 0) + 1;
  }
  async abort() {}
}

class FakeDirectory {
  constructor({ writable = true, quota } = {}) {
    this.files = new Map();
    this.dirs = new Map();
    this.writable = writable;
    this.quota = quota;
    this.kind = 'directory';
  }
  async getDirectoryHandle(name, { create } = {}) {
    if (!this.dirs.has(name)) {
      if (!create) {
        const error = new Error('NotFoundError');
        error.name = 'NotFoundError';
        throw error;
      }
      this.dirs.set(name, new FakeDirectory({ writable: this.writable, quota: this.quota }));
    }
    return this.dirs.get(name);
  }
  async getFileHandle(name, { create } = {}) {
    if (!this.files.has(name)) {
      if (!create) {
        const error = new Error('NotFoundError');
        error.name = 'NotFoundError';
        throw error;
      }
      this.files.set(name, { text: '', lastModified: 0 });
    }
    const file = this.files.get(name);
    const dir = this;
    const handle = {
      kind: 'file',
      name,
      getFile: async () => ({ name, size: file.text.length, lastModified: file.lastModified, text: async () => file.text }),
    };
    if (dir.writable) {
      handle.createWritable = async () => new FakeWritable(file, dir.quota);
    }
    return handle;
  }
  async removeEntry(name) {
    if (!this.files.delete(name) && !this.dirs.delete(name)) {
      const error = new Error('NotFoundError');
      error.name = 'NotFoundError';
      throw error;
    }
  }
  async *entries() {
    for (const [name, dir] of this.dirs) yield [name, dir];
    for (const name of this.files.keys()) yield [name, await this.getFileHandle(name)];
  }
  text(pathName) {
    const parts = pathName.split('/');
    let dir = this;
    for (const part of parts.slice(0, -1)) {
      dir = dir.dirs.get(part);
      if (!dir) return undefined;
    }
    return dir.files.get(parts[parts.length - 1])?.text;
  }
}

function fakeStorage(options) {
  const root = new FakeDirectory(options);
  return { root, storage: { getDirectory: async () => root } };
}

// A just-enough IndexedDB: one database with one object store.
function fakeIndexedDB() {
  const data = new Map();
  const later = (req, value) => {
    setTimeout(() => {
      req.result = value;
      req.onsuccess && req.onsuccess();
    }, 0);
    return req;
  };
  const store = {
    getAllKeys: () => later({}, [...data.keys()]),
    getAll: () => later({}, [...data.values()]),
    put: (value, key) => later({}, data.set(key, value) && key),
    delete: (key) => later({}, data.delete(key) && undefined),
  };
  const db = { transaction: () => ({ objectStore: () => store }), createObjectStore() {} };
  return {
    data,
    indexedDB: {
      open() {
        const req = {};
        setTimeout(() => {
          req.result = db;
          req.onupgradeneeded && req.onupgradeneeded();
          req.onsuccess();
        }, 0);
        return req;
      },
    },
  };
}

function channelBus() {
  const members = [];
  return class FakeChannel {
    constructor() {
      members.push(this);
    }
    postMessage(data) {
      for (const member of members) {
        if (member !== this && member.onmessage) member.onmessage({ data: structuredClone(data) });
      }
    }
  };
}

let clock = 1000;
const now = () => clock++;

test('the folder is read before launch and answers synchronously', async () => {
  const { root, storage } = fakeStorage();
  const notebooks = await root.getDirectoryHandle('notebooks', { create: true });
  await (await notebooks.getDirectoryHandle('autosave', { create: true })).getFileHandle('c_1.json', { create: true });
  notebooks.dirs.get('autosave').files.get('c_1.json').text = '{"cells":[]}';
  notebooks.files.set('trust.json', { text: '{"version":1}', lastModified: 77 });

  const ctx = load();
  const files = await ctx.SqllyNotebookFiles.install({ storage, now });
  assert.equal(files.backend(), 'opfs');
  assert.equal(files.read('autosave/c_1.json'), '{"cells":[]}');
  assert.equal(files.read('trust.json'), '{"version":1}');
  assert.equal(files.modified('trust.json'), 77);
  assert.equal(files.read('missing.json'), null);
  assert.deepEqual([...files.list('autosave/')], ['autosave/c_1.json']);
  assert.equal(notebooks.files.has('.sqlly-probe'), false, 'the write probe is removed');

  files.write('library/nb_1/Sales report.sqllynb', 'v1');
  assert.equal(files.read('library/nb_1/Sales report.sqllynb'), 'v1', 'reads see a write at once');
  assert.ok(files.modified('library/nb_1/Sales report.sqllynb') > 77);
  await files.flush();
  assert.equal(root.text('notebooks/library/nb_1/Sales report.sqllynb'), 'v1');

  files.remove('autosave/c_1.json');
  assert.equal(files.read('autosave/c_1.json'), null);
  await files.flush();
  assert.equal(root.text('notebooks/autosave/c_1.json'), undefined);
  files.remove('autosave/never-written.json');
  await files.flush();
});

test('names that could leave the folder are refused', async () => {
  const ctx = load();
  const files = await ctx.SqllyNotebookFiles.install({ storage: fakeStorage().storage, now });
  for (const bad of ['', '../x', 'a/../b', '/abs', 'a//b', 'a\\b', 'tail/', 'nul\u0000']) {
    assert.throws(() => files.write(bad, 'x'), /invalid storage name/, JSON.stringify(bad));
    assert.equal(ctx.SqllyNotebookFiles.validName(bad), false);
  }
  assert.equal(ctx.SqllyNotebookFiles.validName('library/x/売上.ipynb'), true);
});

test('writes persist in order and only the latest text per file is written', async () => {
  const { root, storage } = fakeStorage();
  const ctx = load();
  const files = await ctx.SqllyNotebookFiles.install({ storage, now });
  files.write('a.json', '1');
  files.write('a.json', '2');
  files.write('b.json', 'b');
  files.write('a.json', '3');
  await files.flush();
  assert.equal(root.text('notebooks/a.json'), '3');
  assert.equal(root.text('notebooks/b.json'), 'b');
  const reopened = await load().SqllyNotebookFiles.install({ storage, now });
  assert.equal(reopened.read('a.json'), '3');
});

test('a write the browser refuses keeps the copy in memory and reports it', async () => {
  const { root, storage } = fakeStorage({ quota: 4 });
  const ctx = load();
  const files = await ctx.SqllyNotebookFiles.install({ storage, now });
  files.write('big.json', 'too large');
  assert.equal(files.read('big.json'), 'too large');
  await files.flush();
  assert.equal(files.lastError(), 'QuotaExceededError');
  assert.equal(root.text('notebooks/big.json'), '');
  files.write('ok.json', 'ok');
  await files.flush();
  assert.equal(files.lastError(), null);
});

test('without OPFS writes the folder lives in IndexedDB, and in memory without either', async () => {
  const { storage } = fakeStorage({ writable: false });
  const idb = fakeIndexedDB();
  const ctx = load();
  const files = await ctx.SqllyNotebookFiles.install({ storage, indexedDB: idb.indexedDB, now });
  assert.equal(files.backend(), 'indexeddb');
  files.write('trust.json', '{}');
  await files.flush();
  assert.equal(idb.data.get('trust.json').text, '{}');
  const again = await load().SqllyNotebookFiles.install({ storage, indexedDB: idb.indexedDB, now });
  assert.equal(again.read('trust.json'), '{}');

  const refusing = { getDirectory: async () => Promise.reject(new Error('SecurityError')) };
  const memory = await load().SqllyNotebookFiles.install({ storage: refusing, now });
  assert.equal(memory.backend(), 'memory');
  memory.write('trust.json', '{}');
  assert.equal(memory.read('trust.json'), '{}');
  await memory.flush();
});

test('other tabs see each write and removal', async () => {
  const Channel = channelBus();
  const { storage } = fakeStorage();
  const first = await load().SqllyNotebookFiles.install({ storage, BroadcastChannel: Channel, now });
  const second = await load().SqllyNotebookFiles.install({ storage, BroadcastChannel: Channel, now });
  first.write('library/nb/n.sqllynb', 'from the first tab');
  assert.equal(second.read('library/nb/n.sqllynb'), 'from the first tab');
  assert.equal(second.modified('library/nb/n.sqllynb'), first.modified('library/nb/n.sqllynb'));
  second.remove('library/nb/n.sqllynb');
  assert.equal(first.read('library/nb/n.sqllynb'), null);
  await first.flush();
  await second.flush();
});

function pickerWindow({ text = '{"format":"sqlly.notebook"}', name = 'q3.sqllynb', size, permission = 'granted' } = {}) {
  const written = [];
  const fileHandle = (fileName) => ({
    name: fileName,
    getFile: async () => ({ name: fileName, size: size ?? text.length, text: async () => text }),
    queryPermission: async () => permission,
    requestPermission: async () => (permission === 'prompt' ? 'granted' : permission),
    createWritable: async () => {
      let buffer = '';
      return {
        write: async (chunk) => {
          buffer += chunk;
        },
        close: async () => written.push([fileName, buffer]),
      };
    },
  });
  return {
    written,
    window: {
      showOpenFilePicker: async (options) => {
        assert.equal(options.multiple, false);
        assert.deepEqual([...options.types[0].accept['application/json']], ['.sqllynb', '.ipynb', '.json']);
        return [fileHandle(name)];
      },
      showSaveFilePicker: async ({ suggestedName }) => fileHandle(suggestedName),
    },
  };
}

test('notebooks open and save through File System Access handles', async () => {
  const page = pickerWindow();
  const files = await load().SqllyNotebookFiles.install({ storage: fakeStorage().storage, window: page.window, now });
  assert.equal(files.canSaveFiles(), true);
  const picked = await files.pickFile('SQLLY notebook');
  assert.equal(picked.name, 'q3.sqllynb');
  assert.equal(picked.text, '{"format":"sqlly.notebook"}');
  assert.match(picked.handle, /^h\d+$/);
  await files.saveFile(picked.handle, 'saved', false);
  assert.deepEqual(page.written.at(-1), ['q3.sqllynb', 'saved']);

  const chosen = await files.chooseSaveFile('report.sqllynb', 'SQLLY notebook');
  assert.equal(chosen.name, 'report.sqllynb');
  assert.equal(page.written.length, 1, 'choosing a file writes nothing');
  await files.saveFile(chosen.handle, 'as', true);
  assert.deepEqual(page.written.at(-1), ['report.sqllynb', 'as']);
  await assert.rejects(files.saveFile('h999', 'x', true), (error) => error.code === 'no_handle');
});

test('a cancelled picker opens nothing and oversized files are refused', async () => {
  const abort = () => {
    const error = new Error('The user aborted a request.');
    error.name = 'AbortError';
    throw error;
  };
  const cancelled = await load().SqllyNotebookFiles.install({
    storage: fakeStorage().storage,
    window: { showOpenFilePicker: async () => abort(), showSaveFilePicker: async () => abort() },
    now,
  });
  assert.equal(await cancelled.pickFile(), null);
  assert.equal(await cancelled.chooseSaveFile('n.sqllynb'), null);

  const huge = pickerWindow({ size: 33 * 1024 * 1024 });
  const files = await load().SqllyNotebookFiles.install({ storage: fakeStorage().storage, window: huge.window, now });
  await assert.rejects(files.pickFile(), (error) => error.code === 'too_large' && error.limitMb === 32);
});

test('writing to a file needs permission, asked for only on an explicit save', async () => {
  const page = pickerWindow({ permission: 'prompt' });
  const files = await load().SqllyNotebookFiles.install({ storage: fakeStorage().storage, window: page.window, now });
  const picked = await files.pickFile();
  await assert.rejects(files.saveFile(picked.handle, 'autosave', false), (error) => error.code === 'permission');
  await files.saveFile(picked.handle, 'explicit', true);
  assert.deepEqual(page.written.at(-1), ['q3.sqllynb', 'explicit']);

  const denied = pickerWindow({ permission: 'denied' });
  const refusing = await load().SqllyNotebookFiles.install({ storage: fakeStorage().storage, window: denied.window, now });
  const file = await refusing.pickFile();
  await assert.rejects(refusing.saveFile(file.handle, 'x', true), (error) => error.code === 'permission');
});

function inputDocument(choose) {
  const body = { children: [], append(node) { this.children.push(node); node.parent = this; } };
  return {
    body,
    createElement(tag) {
      assert.equal(tag, 'input');
      const listeners = {};
      const input = {
        style: {},
        files: null,
        addEventListener: (type, fn) => (listeners[type] = fn),
        remove() {
          body.children = body.children.filter((child) => child !== input);
        },
        click() {
          setTimeout(() => choose(input, listeners), 0);
        },
      };
      return input;
    },
  };
}

test('without a picker a file input opens the notebook, and cancelling it opens nothing', async () => {
  const doc = inputDocument((input, listeners) => {
    assert.equal(input.type, 'file');
    assert.equal(input.accept, '.sqllynb,.ipynb,.json');
    input.files = [{ name: 'legacy.sqlnb.json', size: 2, text: async () => '{}' }];
    listeners.change();
  });
  const files = await load().SqllyNotebookFiles.install({ storage: fakeStorage().storage, window: {}, document: doc, now });
  assert.equal(files.canSaveFiles(), false);
  const picked = await files.pickFile();
  assert.deepEqual({ ...picked }, { name: 'legacy.sqlnb.json', text: '{}', handle: null });
  assert.equal(doc.body.children.length, 0, 'the input is removed');
  await assert.rejects(files.chooseSaveFile('n.sqllynb'), (error) => error.code === 'unsupported');

  const cancelling = inputDocument((_input, listeners) => listeners.cancel());
  const other = await load().SqllyNotebookFiles.install({ storage: fakeStorage().storage, window: {}, document: cancelling, now });
  assert.equal(await other.pickFile(), null);
});

test('the page installs itself and boot.js waits for it', () => {
  const ctx = load({
    navigator: { storage: fakeStorage().storage },
    document: inputDocument(() => {}),
  });
  assert.equal(typeof ctx.sqllyNotebookFilesReady.then, 'function');
  return ctx.sqllyNotebookFilesReady.then((api) => {
    assert.equal(ctx.sqllyNotebookFiles, api);
    assert.equal(api.backend(), 'opfs');
    const boot = fs.readFileSync(path.join(__dirname, '../www/boot.js'), 'utf8');
    assert.match(boot, /await notebookFilesReady\(\);\s*await launchApplication/);
    assert.doesNotMatch(source, /import\.meta|^\s*import\s/m);
  });
});
