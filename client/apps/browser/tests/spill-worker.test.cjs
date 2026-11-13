// The spill worker (www/spill-worker.js) over an in-memory stand-in for the
// Origin Private File System: appends land in order, page reads return the
// exact byte run of the rows asked for, and removal and the startup sweep
// leave other tabs' open files alone.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../www/spill-worker.js'), 'utf8');

class FakeSyncHandle {
  constructor(file) {
    this.file = file;
    file.locked = true;
  }
  getSize() {
    return this.file.bytes.length;
  }
  truncate(size) {
    this.file.bytes = this.file.bytes.slice(0, size);
  }
  write(bytes, { at }) {
    if (this.file.quota !== undefined && at + bytes.length > this.file.quota) {
      throw new Error('QuotaExceededError');
    }
    const end = Math.max(this.file.bytes.length, at + bytes.length);
    const grown = new Uint8Array(end);
    grown.set(this.file.bytes);
    grown.set(bytes, at);
    this.file.bytes = grown;
    return bytes.length;
  }
  read(out, { at }) {
    const part = this.file.bytes.subarray(at, at + out.length);
    out.set(part);
    return part.length;
  }
  close() {
    this.file.locked = false;
  }
}

class FakeDirectory {
  constructor() {
    this.files = new Map();
    this.dirs = new Map();
  }
  async getDirectoryHandle(name) {
    if (!this.dirs.has(name)) this.dirs.set(name, new FakeDirectory());
    return this.dirs.get(name);
  }
  async getFileHandle(name, { create } = {}) {
    if (!this.files.has(name)) {
      if (!create) throw new Error('NotFoundError');
      this.files.set(name, { bytes: new Uint8Array(0), locked: false });
    }
    const file = this.files.get(name);
    return {
      createSyncAccessHandle: async () => {
        if (file.locked) throw new Error('NoModificationAllowedError');
        return new FakeSyncHandle(file);
      },
    };
  }
  async removeEntry(name) {
    const file = this.files.get(name);
    if (!file) throw new Error('NotFoundError');
    if (file.locked) throw new Error('NoModificationAllowedError');
    this.files.delete(name);
  }
  async *keys() {
    yield* [...this.files.keys()];
  }
}

function startWorker() {
  const root = new FakeDirectory();
  const replies = new Map();
  const waiting = new Map();
  const self = {
    postMessage(message) {
      const resolve = waiting.get(message.id);
      if (resolve) {
        waiting.delete(message.id);
        resolve(message);
      } else {
        replies.set(message.id, message);
      }
    },
  };
  class FileSystemFileHandle {}
  FileSystemFileHandle.prototype.createSyncAccessHandle = () => {};
  const context = vm.createContext({
    self,
    navigator: {
      storage: {
        getDirectory: async () => root,
        estimate: async () => ({ quota: 1000, usage: 250 }),
      },
    },
    FileSystemFileHandle,
    DataView,
    Uint8Array,
    ArrayBuffer,
    Number,
    Promise,
    Error,
    Map,
    String,
    Math,
  });
  vm.runInContext(source, context);
  let nextId = 1;
  const call = (op, args = {}) =>
    new Promise((resolve) => {
      const id = nextId++;
      waiting.set(id, resolve);
      self.onmessage({ data: { id, op, args } });
    });
  const send = (op, args) => self.onmessage({ data: { id: 0, op, args } });
  return { call, send, root };
}

// A spill of `rows` records, record i being i + 1 bytes of value i, in the
// layout sqlly-core writes: 8-byte headers, then records / u64 offsets.
function spillBytes(rows) {
  const records = [];
  const offsets = [];
  let at = 8;
  for (let i = 0; i < rows; i += 1) {
    offsets.push(at);
    records.push(new Uint8Array(i + 1).fill(i));
    at += i + 1;
  }
  const data = new Uint8Array(at);
  data.set([0x53, 0x51, 0x53, 0x50, 2, 0, 0, 0]);
  let cursor = 8;
  for (const record of records) {
    data.set(record, cursor);
    cursor += record.length;
  }
  const index = new Uint8Array(8 + rows * 8);
  index.set([0x53, 0x51, 0x53, 0x58, 2, 0, 0, 0]);
  const view = new DataView(index.buffer);
  offsets.forEach((offset, i) => view.setBigUint64(8 + i * 8, BigInt(offset), true));
  return { data, index };
}

function rowsOf(buffer) {
  // Record i is i + 1 bytes of value i: rebuild the row numbers.
  const bytes = new Uint8Array(buffer);
  const rows = [];
  let at = 0;
  while (at < bytes.length) {
    const row = bytes[at];
    rows.push(row);
    at += row + 1;
  }
  return rows;
}

test('reports support for OPFS sync handles', async () => {
  const worker = startWorker();
  const reply = await worker.call('supported');
  assert.equal(reply.ok, true);
  assert.equal(reply.value, true);
});

test('appends in order and pages back exact row runs', async () => {
  const worker = startWorker();
  const { data, index } = spillBytes(40);
  worker.send('create', { name: 'sqlly-spill-a.bin' });
  // Two appends that split both files mid-record, as 1 MiB shipments do.
  worker.send('append', { name: 'sqlly-spill-a.bin', data: data.slice(0, 100).buffer, index: index.slice(0, 50).buffer });
  worker.send('append', { name: 'sqlly-spill-a.bin', data: data.slice(100).buffer, index: index.slice(50).buffer });

  const middle = await worker.call('page', { name: 'sqlly-spill-a.bin', start: 10, end: 14 });
  assert.ok(middle.ok, middle.error);
  assert.deepEqual(rowsOf(middle.value), [10, 11, 12, 13]);

  const tail = await worker.call('page', { name: 'sqlly-spill-a.bin', start: 37, end: 100 });
  assert.deepEqual(rowsOf(tail.value), [37, 38, 39], 'clamped to the rows written');

  const empty = await worker.call('page', { name: 'sqlly-spill-a.bin', start: 50, end: 60 });
  assert.equal(empty.value.byteLength, 0);

  const body = await worker.call('body', { name: 'sqlly-spill-a.bin' });
  assert.deepEqual(rowsOf(body.value), [...Array(40).keys()]);
});

test('a failed write makes later reads of that spill fail', async () => {
  const worker = startWorker();
  worker.send('create', { name: 'sqlly-spill-q.bin' });
  // Wait for create, then cap the data file.
  await worker.call('supported');
  worker.root.dirs.get('sqlly-spill').files.get('sqlly-spill-q.bin').quota = 16;
  const { data, index } = spillBytes(10);
  worker.send('append', { name: 'sqlly-spill-q.bin', data: data.buffer, index: index.buffer });
  const page = await worker.call('page', { name: 'sqlly-spill-q.bin', start: 0, end: 1 });
  assert.equal(page.ok, false);
  assert.match(page.error, /QuotaExceeded/);
});

test('remove deletes both files; the sweep spares open spills', async () => {
  const worker = startWorker();
  for (const name of ['sqlly-spill-a.bin', 'sqlly-spill-b.bin']) {
    worker.send('create', { name });
  }
  await worker.call('supported');
  const dir = worker.root.dirs.get('sqlly-spill');
  // Left by a closed tab, and held open by another tab.
  dir.files.set('sqlly-spill-old.bin', { bytes: new Uint8Array(4), locked: false });
  dir.files.set('sqlly-spill-old.bin.idx', { bytes: new Uint8Array(4), locked: false });
  dir.files.set('sqlly-spill-other.bin', { bytes: new Uint8Array(4), locked: true });

  worker.send('remove', { name: 'sqlly-spill-a.bin' });
  const swept = await worker.call('sweep');
  assert.equal(swept.value, 2, 'the closed tab’s two files');
  assert.deepEqual(
    [...dir.files.keys()].sort(),
    ['sqlly-spill-b.bin', 'sqlly-spill-b.bin.idx', 'sqlly-spill-other.bin'],
  );
});

test('estimate passes the storage quota through', async () => {
  const worker = startWorker();
  const estimate = await worker.call('estimate');
  assert.equal(estimate.value.quota, 1000);
  assert.equal(estimate.value.usage, 250);
});

test('unknown spills and operations are errors', async () => {
  const worker = startWorker();
  const missing = await worker.call('page', { name: 'nope', start: 0, end: 1 });
  assert.equal(missing.ok, false);
  const unknown = await worker.call('frobnicate');
  assert.match(unknown.error, /unknown spill op/);
});
