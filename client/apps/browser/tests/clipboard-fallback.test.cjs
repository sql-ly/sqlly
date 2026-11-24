// The clipboard fallback (www/clipboard-fallback.js) against a stubbed page.
// Over plain HTTP (not a secure context) browsers leave `navigator.clipboard`
// undefined, and gpui's web platform copies with
// `navigator.clipboard.writeText` and nothing else, so Copy and Cut did
// nothing. The fallback supplies a `writeText` that copies through a hidden
// textarea and `document.execCommand('copy')`, and also falls back when a
// present clipboard refuses the write.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../www/clipboard-fallback.js'), 'utf8');

class FakeElement {
  constructor(document, tag) {
    this.document = document;
    this.tagName = tag.toUpperCase();
    this.attributes = {};
    this.style = {};
    this.value = '';
    this.parent = null;
  }
  setAttribute(name, value) {
    this.attributes[name] = value;
  }
  focus() {
    this.document.activeElement = this;
  }
  select() {
    this.document.selection = this.value;
  }
  remove() {
    if (this.parent) {
      this.parent.children = this.parent.children.filter((child) => child !== this);
      this.parent = null;
    }
  }
}

function fakePage({ clipboard, execCommandWorks = true } = {}) {
  const document = {
    activeElement: null,
    selection: null,
    copied: [],
    createElement(tag) {
      return new FakeElement(document, tag);
    },
    execCommand(command) {
      if (command !== 'copy' || !execCommandWorks || document.selection === null) {
        return false;
      }
      document.copied.push(document.selection);
      return true;
    },
  };
  document.body = new FakeElement(document, 'body');
  document.body.children = [];
  document.body.appendChild = (child) => {
    child.parent = document.body;
    document.body.children.push(child);
  };
  const gpuiInput = new FakeElement(document, 'input');
  gpuiInput.focus();
  const navigator = {};
  if (clipboard) {
    navigator.clipboard = clipboard;
  }
  const context = { document, navigator, isSecureContext: Boolean(clipboard) };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(source, context);
  return { context, document, navigator, gpuiInput };
}

test('copies through execCommand when navigator.clipboard is missing', async () => {
  const { document, navigator, gpuiInput } = fakePage();
  assert.equal(typeof navigator.clipboard?.writeText, 'function');
  await navigator.clipboard.writeText('SELECT 1;\nGO');
  assert.deepEqual(document.copied, ['SELECT 1;\nGO']);
  // The helper textarea is gone and the app's input has focus back.
  assert.equal(document.body.children.length, 0);
  assert.equal(document.activeElement, gpuiInput);
});

test('the call is synchronous so it stays inside the user gesture', () => {
  const { document, navigator } = fakePage();
  navigator.clipboard.writeText('now');
  assert.deepEqual(document.copied, ['now']);
});

test('rejects instead of throwing when execCommand cannot copy either', async () => {
  const { document, navigator } = fakePage({ execCommandWorks: false });
  await assert.rejects(navigator.clipboard.writeText('x'));
  assert.equal(document.body.children.length, 0);
});

test('reads report unavailable rather than throwing on a missing method', async () => {
  const { navigator } = fakePage();
  await assert.rejects(navigator.clipboard.read(), { name: 'NotAllowedError' });
  await assert.rejects(navigator.clipboard.readText(), { name: 'NotAllowedError' });
});

test('a working clipboard is used as-is', async () => {
  const written = [];
  const clipboard = {
    writeText(text) {
      written.push(text);
      return Promise.resolve();
    },
  };
  const { document, navigator } = fakePage({ clipboard });
  await navigator.clipboard.writeText('kept');
  assert.deepEqual(written, ['kept']);
  assert.deepEqual(document.copied, []);
});

test('falls back when the clipboard rejects the write', async () => {
  const clipboard = {
    writeText() {
      const error = new Error('Write permission denied.');
      error.name = 'NotAllowedError';
      return Promise.reject(error);
    },
  };
  const { document, navigator } = fakePage({ clipboard });
  await navigator.clipboard.writeText('rescued');
  assert.deepEqual(document.copied, ['rescued']);
});

test('falls back when the clipboard throws synchronously', async () => {
  const clipboard = {
    writeText() {
      throw new TypeError('Illegal invocation');
    },
  };
  const { document, navigator } = fakePage({ clipboard });
  await navigator.clipboard.writeText('also rescued');
  assert.deepEqual(document.copied, ['also rescued']);
});
