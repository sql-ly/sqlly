const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const ORIGIN = 'https://sqlly.test';
const CHANNEL = 'sqlly.translations.v1';
const LANGUAGES = ['en', 'es', 'zh-CN', 'ja', 'pt-BR', 'ko', 'de', 'ru', 'fr', 'zh-TW'];
const catalogs = Object.fromEntries(
  LANGUAGES.map((lang) => [
    lang,
    JSON.parse(
      fs.readFileSync(path.join(__dirname, `../../../crates/sqlly-i18n/translations/${lang}.json`), 'utf8'),
    ),
  ]),
);
const lookup = (catalog, key) => key.split('.').reduce((node, part) => node && node[part], catalog)?.value;
const bootSource = fs
  .readFileSync(path.join(__dirname, '../www/boot.js'), 'utf8')
  .replace(/\nboot\(\);\s*$/, '\n');

function element() {
  const classes = new Set();
  return {
    textContent: '',
    style: {},
    removed: false,
    children: [],
    classList: {
      add: (name) => classes.add(name),
      remove: (name) => classes.delete(name),
      contains: (name) => classes.has(name),
    },
    remove() {
      this.removed = true;
    },
    replaceChildren() {
      this.children = [];
    },
    append(...children) {
      this.children.push(...children);
    },
    addEventListener() {},
    focus() {},
  };
}

function createHarness({ ready = true } = {}) {
  const elements = {
    loading: element(),
    progress: element(),
    status: element(),
    fact: element(),
    go: element(),
  };
  const lead = element();
  const listeners = new Map();
  const posted = [];
  const storageWrites = [];
  const parent = {
    postMessage(data, targetOrigin) {
      posted.push({ data, targetOrigin });
    },
  };
  const document = {
    title: '',
    documentElement: { lang: 'en' },
    body: { contains: (node) => Boolean(node) && !node.removed },
    getElementById: (id) => elements[id] || null,
    querySelector: (selector) => (selector === '.native-note .lead' ? lead : null),
    createElement: () => element(),
  };
  const storage = (name) => ({
    setItem(...args) {
      storageWrites.push([name, ...args]);
    },
  });
  const context = {
    console: { warn() {}, error() {} },
    document,
    location: { origin: ORIGIN, search: '' },
    localStorage: storage('localStorage'),
    sessionStorage: storage('sessionStorage'),
    parent,
    TextEncoder,
    URLSearchParams,
    setInterval: () => 1,
    clearInterval() {},
    setTimeout() {},
    clearTimeout() {},
  };
  context.window = context;
  context.addEventListener = (type, listener) => listeners.set(type, listener);
  vm.createContext(context);
  vm.runInContext(
    `${bootSource}\nglobalThis.__bridgeTest = {
      setWasm(module) { wasmModule = module; wasmReady = true; },
      markReady(module) { wasmModule = module; wasmReady = true; settleQueuedCommands(null); },
      failBoot(message) { settleQueuedCommands(message); },
      launchApplication,
      notifyParentReady,
      setPhase,
      setProgress,
      state() { return { activeLanguage, wasmReady }; },
    };`,
    context,
  );

  const calls = [];
  let replacementError = null;
  let replacementResult;
  // Mirrors the WASM host: the active catalog is the language's compiled
  // messages under the host overrides; a language switch drops overrides
  // and keys the host reports as skipped are never applied.
  const catalogState = { language: 'en', overrides: {} };
  const withoutKeys = (overrides, keys) => {
    const copy = JSON.parse(JSON.stringify(overrides));
    for (const key of keys) {
      const parts = key.split('.');
      const last = parts.pop();
      const parent = parts.reduce((node, part) => node && node[part], copy);
      if (parent) delete parent[last];
    }
    return copy;
  };
  const wasm = {
    set_language(language) {
      calls.push(['set_language', language]);
      catalogState.language = language;
      catalogState.overrides = {};
    },
    replace_translation_overrides(json) {
      calls.push(['replace_translation_overrides', json]);
      if (replacementError) {
        const error = replacementError;
        replacementError = null;
        throw error;
      }
      const skipped = Array.isArray(replacementResult) ? replacementResult : [];
      catalogState.overrides = withoutKeys(JSON.parse(json), skipped);
      return replacementResult;
    },
    clear_translation_overrides() {
      calls.push(['clear_translation_overrides']);
      catalogState.overrides = {};
    },
    translation_message(key, json) {
      let result =
        lookup(catalogState.overrides, key) || lookup(catalogs[catalogState.language], key) || key;
      for (const [name, value] of Object.entries(JSON.parse(json))) {
        result = result.replaceAll(`{${name}}`, value);
      }
      return result;
    },
  };
  if (ready) {
    context.__bridgeTest.setWasm(wasm);
  }

  return {
    api: context.sqllyTranslations,
    calls,
    catalogState,
    wasm,
    context,
    document,
    elements,
    lead,
    posted,
    storageWrites,
    markReady() {
      context.__bridgeTest.markReady(wasm);
    },
    failBoot(message) {
      context.__bridgeTest.failBoot(message);
    },
    returnFromReplacement(value) {
      replacementResult = value;
    },
    failNextReplacement(error) {
      replacementError = error;
    },
    send(data, { origin = ORIGIN, source = parent } = {}) {
      listeners.get('message')({ data, origin, source });
    },
    ready() {
      context.__bridgeTest.notifyParentReady();
    },
    launch: context.__bridgeTest.launchApplication,
    setPhase: context.__bridgeTest.setPhase,
    setProgress: context.__bridgeTest.setProgress,
    state: context.__bridgeTest.state,
  };
}

function leaf(value, location = 'Browser boot: translation test entry.') {
  return { value, location };
}

function catalog() {
  return {
    settings: {
      appearance: {
        language: {
          title: leaf('Idioma'),
        },
      },
    },
  };
}

function command(type, requestId = 'request-1') {
  return { channel: CHANNEL, requestId, type };
}

function plain(value) {
  return JSON.parse(JSON.stringify(value));
}

test('browser chrome uses the active WASM catalog after switch and clear', () => {
  const harness = createHarness();
  let language = 'en';
  let overrides = {};
  harness.context.__bridgeTest.setWasm({
    set_language(next) { language = next; overrides = {}; },
    replace_translation_overrides(json) { overrides = JSON.parse(json); return []; },
    clear_translation_overrides() { overrides = {}; },
    translation_message(key, json) {
      let result = lookup(overrides, key) || lookup(catalogs[language], key);
      for (const [name, value] of Object.entries(JSON.parse(json))) result = result.replaceAll('{' + name + '}', value);
      return result;
    },
  });
  harness.api.setLanguage('es');
  const spanish = lookup(catalogs.es, 'browser.boot.preview_title');
  assert.equal(harness.document.title, spanish);
  harness.api.replaceOverrides({ browser: { boot: { preview_title: leaf('Custom title') } } });
  assert.equal(harness.document.title, 'Custom title');
  harness.api.setLanguage('en');
  assert.equal(harness.document.title, lookup(catalogs.en, 'browser.boot.preview_title'));
  harness.api.setLanguage('es');
  harness.api.replaceOverrides({ browser: { boot: { preview_title: leaf('Custom title') } } });
  harness.api.clearOverrides();
  assert.equal(harness.document.title, spanish);
});

const MAX_TRANSLATION_BYTES = 8 * 1024 * 1024;

function catalogOfSize(bytes) {
  const overrides = { browser: { boot: { preparing: leaf('') } } };
  const emptySize = Buffer.byteLength(JSON.stringify(overrides));
  assert(bytes >= emptySize);
  overrides.browser.boot.preparing.value = 'x'.repeat(bytes - emptySize);
  assert.equal(Buffer.byteLength(JSON.stringify(overrides)), bytes);
  return overrides;
}

test('public API applies all commands synchronously without persistence', (t) => {
  const harness = createHarness();
  const overrides = catalog();

  harness.api.setLanguage('es');
  assert.equal(harness.document.documentElement.lang, 'es');
  harness.api.replaceOverrides(overrides);
  harness.api.clearOverrides();

  assert.deepEqual(harness.calls, [
    ['set_language', 'es'],
    ['replace_translation_overrides', JSON.stringify(overrides)],
    ['clear_translation_overrides'],
  ]);
  assert.deepEqual(plain(harness.state()), { activeLanguage: 'es', wasmReady: true });
  assert.deepEqual(harness.storageWrites, []);
  t.mock.method(harness.context.localStorage, 'setItem', () => {
    assert.fail('host language changes must not persist preferences');
  });
  harness.api.setLanguage('en');
});

test('public API accepts every supported application language', () => {
  for (const language of LANGUAGES) {
    const harness = createHarness();
    harness.api.setLanguage(language);
    assert.equal(harness.document.documentElement.lang, language);
    assert.deepEqual(harness.calls, [['set_language', language]]);
  }
});

test('replacement sends a complete payload rather than merging at the API boundary', () => {
  const harness = createHarness();
  harness.api.replaceOverrides(catalog());
  const replacement = { browser: { boot: { go_button: leaf('Continuar') } } };
  harness.api.replaceOverrides(replacement);

  assert.equal(JSON.parse(harness.calls[0][1]).settings.appearance.language.title.value, 'Idioma');
  assert.deepEqual(JSON.parse(harness.calls[1][1]), replacement);
  assert.equal(harness.elements.go.textContent, 'Continuar');
  harness.api.clearOverrides();
  assert.equal(harness.elements.go.textContent, 'Go');
});

test('public API rejects invalid languages and catalogs before WASM', () => {
  const harness = createHarness();
  for (const language of ['EN', 'unsupported', '', null, 1]) {
    assert.throws(() => harness.api.setLanguage(language));
  }
  const invalid = [
    null,
    [],
    { Bad: leaf('x') },
    { _bad: leaf('x') },
    { bad__key: leaf('x') },
    { bad_: leaf('x') },
    { group: {} },
    { group: { value: 'x' } },
    { group: { value: 'x', location: 'here', extra: true } },
    { group: leaf('') },
    { group: leaf('   ') },
    { group: leaf('x', '') },
    { group: { value: 1, location: 'here' } },
  ];
  for (const overrides of invalid) {
    assert.throws(() => harness.api.replaceOverrides(overrides));
  }
  assert.deepEqual(harness.calls, []);
});

test('multi-MB catalogs and exactly 8 MiB are accepted, while 8 MiB plus one byte is rejected', () => {
  const harness = createHarness();
  const projectedCatalog = catalogOfSize(4 * 1024 * 1024);
  const boundaryCatalog = catalogOfSize(MAX_TRANSLATION_BYTES);
  const oversized = catalogOfSize(MAX_TRANSLATION_BYTES + 1);

  harness.api.replaceOverrides(projectedCatalog);
  harness.api.replaceOverrides(boundaryCatalog);
  assert.equal(Buffer.byteLength(harness.calls[0][1]), 4 * 1024 * 1024);
  assert.equal(Buffer.byteLength(harness.calls[1][1]), MAX_TRANSLATION_BYTES);
  assert.throws(() => harness.api.replaceOverrides(oversized), /8 MiB/);
  assert.equal(harness.calls.length, 2);
});

test('bridge accepts all commands and correlates acknowledgements', () => {
  const harness = createHarness();
  const overrides = catalog();
  harness.send({ ...command('set_language', 'language-1'), language: 'es' });
  harness.send({ ...command('replace_overrides', 'catalog-1'), overrides });
  harness.send(command('clear_overrides', 'clear-1'));

  assert.deepEqual(harness.calls, [
    ['set_language', 'es'],
    ['replace_translation_overrides', JSON.stringify(overrides)],
    ['clear_translation_overrides'],
  ]);
  assert.deepEqual(
    plain(harness.posted.map(({ data, targetOrigin }) => ({ data, targetOrigin }))),
    [
      {
        data: { channel: CHANNEL, requestId: 'language-1', type: 'ack', requestType: 'set_language', ok: true },
        targetOrigin: ORIGIN,
      },
      {
        data: {
          channel: CHANNEL,
          requestId: 'catalog-1',
          type: 'ack',
          requestType: 'replace_overrides',
          ok: true,
          skipped: [],
        },
        targetOrigin: ORIGIN,
      },
      {
        data: { channel: CHANNEL, requestId: 'clear-1', type: 'ack', requestType: 'clear_overrides', ok: true },
        targetOrigin: ORIGIN,
      },
    ],
  );
});

test('bridge rejects malformed commands and oversize messages with correlated errors', () => {
  const harness = createHarness();
  const malformed = [
    { ...command('set_language'), language: 'unsupported' },
    { ...command('set_language'), language: 'en', extra: true },
    command('set_language'),
    { ...command('replace_overrides'), overrides: { bad: leaf('') } },
    { ...command('replace_overrides'), overrides: 'not an object' },
    { ...command('replace_overrides'), overrides: catalogOfSize(MAX_TRANSLATION_BYTES + 1) },
    { ...command('clear_overrides'), overrides: {} },
    command('unknown'),
  ];
  malformed.forEach((data, index) => harness.send({ ...data, requestId: `bad-${index}` }));

  assert.equal(harness.posted.length, malformed.length);
  harness.posted.forEach(({ data, targetOrigin }, index) => {
    assert.equal(targetOrigin, ORIGIN);
    assert.equal(data.channel, CHANNEL);
    assert.equal(data.requestId, `bad-${index}`);
    assert.equal(data.type, 'ack');
    assert.equal(data.ok, false);
    assert.equal(typeof data.error, 'string');
  });
  assert.deepEqual(harness.calls, []);
});

test('rejections carry a stable code and parameters the manager can translate', () => {
  const harness = createHarness();
  const cases = [
    [{ ...command('set_language'), language: 'xx' }, 'language_invalid', {}],
    [{ ...command('set_language'), language: 'en', extra: true }, 'command_shape', {}],
    [command('unknown'), 'command_unknown', {}],
    [{ ...command('replace_overrides'), overrides: 'not an object' }, 'catalog_invalid', {}],
    [{ ...command('replace_overrides'), overrides: catalogOfSize(MAX_TRANSLATION_BYTES + 1) }, 'payload_too_large', {}],
    [{ ...command('replace_overrides'), overrides: { Bad: leaf('x') } }, 'catalog_key_invalid', { key: 'Bad' }],
    [{ ...command('replace_overrides'), overrides: { menu: 'x' } }, 'catalog_entry_invalid', { path: 'menu' }],
    [{ ...command('replace_overrides'), overrides: { menu: leaf('') } }, 'catalog_value_invalid', { path: 'menu' }],
    [{ ...command('replace_overrides'), overrides: { menu: { value: 'x', location: ' ' } } }, 'catalog_location_invalid', { path: 'menu' }],
  ];
  cases.forEach(([data], index) => harness.send({ ...data, requestId: `coded-${index}` }));
  cases.forEach(([, code, params], index) => {
    const ack = harness.posted[index].data;
    assert.equal(ack.code, code, `case ${index}`);
    assert.deepEqual(plain(ack.params), params, `case ${index}`);
    assert.equal(typeof ack.error, 'string', 'the English text stays for older managers');
  });
});

test('bridge ignores foreign origins, sources, channels, and invalid request ids', () => {
  const harness = createHarness();
  const valid = { ...command('set_language'), language: 'en' };
  harness.send(valid, { origin: 'https://attacker.test' });
  harness.send(valid, { source: { postMessage() {} } });
  harness.send({ ...valid, channel: 'other.channel' });
  harness.send({ ...valid, requestId: '' });
  harness.send({ ...valid, requestId: 42 });
  harness.send(null);

  assert.deepEqual(harness.calls, []);
  assert.deepEqual(harness.posted, []);
});

test('failed WASM updates are acknowledged and do not alter effective boot text', () => {
  const harness = createHarness();
  harness.api.setLanguage('es');
  harness.api.replaceOverrides({ browser: { boot: { go_button: leaf('Continuar') } } });
  harness.calls.length = 0;
  harness.posted.length = 0;
  harness.failNextReplacement(new Error('Unknown translation key'));

  harness.send({ ...command('replace_overrides'), overrides: { unknown: leaf('x') } });

  assert.equal(harness.posted[0].data.ok, false);
  assert.equal(harness.posted[0].data.error, 'Unknown translation key');
  assert.equal(harness.elements.go.textContent, 'Continuar');
  assert.deepEqual(harness.storageWrites, []);
});

test('ready notification uses the installed channel and same origin', () => {
  const harness = createHarness();
  harness.ready();
  assert.deepEqual(plain(harness.posted), [
    {
      data: { channel: CHANNEL, type: 'ready' },
      targetOrigin: ORIGIN,
    },
  ]);
});

test('effective catalog localizes static chrome and parameterized progress', () => {
  const harness = createHarness();
  const overrides = {
    browser: {
      boot: {
        preparing: leaf('Preparando…'),
        preview_title: leaf('Vista previa de SQLLY en el navegador'),
        preview_lead: leaf('Vista previa de SQLLY. No puede acceder a sus servidores.'),
        go_button: leaf('Continuar'),
        loading_application: leaf('Cargando la aplicación…'),
        downloading_progress: leaf('Descargando {received_mb} de {total_mb} MB'),
      },
    },
  };
  harness.api.replaceOverrides(overrides);
  assert.equal(harness.elements.status.textContent, 'Preparando…');
  assert.equal(harness.elements.go.textContent, 'Continuar');
  assert.equal(harness.lead.textContent, 'Vista previa de SQLLY. No puede acceder a sus servidores.');
  assert.equal(harness.document.title, 'Vista previa de SQLLY en el navegador');
  harness.setPhase('browser.boot.loading_application', 'Loading application…');
  assert.equal(harness.elements.status.textContent, 'Cargando la aplicación…');
  harness.setProgress(1048576, 2097152);
  assert.equal(harness.elements.status.textContent, 'Descargando 1.0 de 2.0 MB');
  harness.api.clearOverrides();
  assert.equal(harness.elements.status.textContent, 'Downloading 1.0 / 2.0 MB');
});

test('commands sent before WASM is ready are queued, coalesced, and flushed in order', () => {
  const harness = createHarness({ ready: false });
  const first = catalog();
  const second = { browser: { boot: { go_button: leaf('Continuar') } } };
  harness.send({ ...command('set_language', 'lang-1'), language: 'en' });
  harness.send({ ...command('replace_overrides', 'cat-1'), overrides: first });
  harness.send({ ...command('set_language', 'lang-2'), language: 'es' });
  harness.send({ ...command('replace_overrides', 'cat-2'), overrides: second });

  assert.deepEqual(harness.calls, []);
  assert.deepEqual(harness.posted, [], 'nothing is acknowledged (or rejected) before ready');

  harness.markReady();

  assert.deepEqual(harness.calls, [
    ['set_language', 'es'],
    ['replace_translation_overrides', JSON.stringify(second)],
  ]);
  assert.equal(harness.document.documentElement.lang, 'es');
  assert.equal(harness.elements.go.textContent, 'Continuar');
  const acks = plain(harness.posted.map(({ data }) => data));
  assert.deepEqual(
    acks.map((ack) => [ack.requestId, ack.requestType, ack.ok]),
    [
      ['lang-1', 'set_language', true],
      ['lang-2', 'set_language', true],
      ['cat-1', 'replace_overrides', true],
      ['cat-2', 'replace_overrides', true],
    ],
  );
  assert.deepEqual(acks[3].skipped, []);
  harness.posted.forEach(({ targetOrigin }) => assert.equal(targetOrigin, ORIGIN));
});

test('queued commands keep set_language ahead of the overrides sent after it', () => {
  const harness = createHarness({ ready: false });
  harness.send({ ...command('replace_overrides', 'cat-1'), overrides: catalog() });
  harness.send({ ...command('set_language', 'lang-1'), language: 'es' });
  harness.send({ ...command('replace_overrides', 'cat-2'), overrides: catalog() });
  harness.markReady();
  assert.deepEqual(
    harness.calls.map(([name]) => name),
    ['set_language', 'replace_translation_overrides'],
  );
});

test('invalid commands are rejected immediately even before WASM is ready', () => {
  const harness = createHarness({ ready: false });
  harness.send({ ...command('set_language', 'bad-lang'), language: 'unsupported' });
  harness.send({ ...command('replace_overrides', 'bad-cat'), overrides: { bad: leaf('') } });
  assert.deepEqual(
    plain(harness.posted.map(({ data }) => [data.requestId, data.ok])),
    [
      ['bad-lang', false],
      ['bad-cat', false],
    ],
  );
  harness.markReady();
  assert.deepEqual(harness.calls, []);
  assert.equal(harness.posted.length, 2);
});

test('queued commands receive the real WASM result after flush', () => {
  const harness = createHarness({ ready: false });
  harness.failNextReplacement(new Error('Unknown translation key'));
  harness.send({ ...command('replace_overrides', 'cat-1'), overrides: catalog() });
  harness.markReady();
  assert.deepEqual(plain(harness.posted[0].data), {
    channel: CHANNEL,
    requestId: 'cat-1',
    type: 'ack',
    requestType: 'replace_overrides',
    ok: false,
    error: 'Unknown translation key',
  });
});

test('queued commands are rejected when WASM boot fails', () => {
  const harness = createHarness({ ready: false });
  harness.send({ ...command('set_language', 'lang-1'), language: 'es' });
  harness.failBoot('download failed');
  assert.deepEqual(plain(harness.posted[0].data), {
    channel: CHANNEL,
    requestId: 'lang-1',
    type: 'ack',
    requestType: 'set_language',
    ok: false,
    error: 'download failed',
  });
});

test('the pre-ready queue is bounded', () => {
  const harness = createHarness({ ready: false });
  for (let index = 0; index < 65; index += 1) {
    harness.send({ ...command('set_language', `lang-${index}`), language: 'en' });
  }
  assert.equal(harness.posted.length, 1);
  assert.equal(harness.posted[0].data.requestId, 'lang-64');
  assert.equal(harness.posted[0].data.ok, false);
  assert.match(harness.posted[0].data.error, /Too many/);
  assert.equal(harness.posted[0].data.code, 'queue_full');
  harness.markReady();
  assert.equal(harness.calls.length, 1);
  assert.equal(harness.posted.length, 65);
});

test('acknowledgements pass through the keys the WASM host skipped, in every return shape', () => {
  const shapes = [
    [undefined, []],
    [null, []],
    [['settings.removed_key'], ['settings.removed_key']],
    ['["settings.removed_key","browser.boot.gone"]', ['settings.removed_key', 'browser.boot.gone']],
    [{ skipped: ['settings.removed_key'] }, ['settings.removed_key']],
    ['not json', []],
  ];
  for (const [returned, expected] of shapes) {
    const harness = createHarness();
    harness.returnFromReplacement(returned);
    harness.send({ ...command('replace_overrides', 'cat-1'), overrides: catalog() });
    assert.equal(harness.posted[0].data.ok, true);
    assert.deepEqual(plain(harness.posted[0].data.skipped), expected);
  }
});

test('skipped keys do not drive boot text and the public API reports them', () => {
  const harness = createHarness();
  harness.returnFromReplacement(['browser.boot.go_button']);
  const result = harness.api.replaceOverrides({
    browser: { boot: { go_button: leaf('Continuar'), preparing: leaf('Preparando…') } },
  });
  assert.deepEqual(plain(result), { skipped: ['browser.boot.go_button'] });
  assert.equal(harness.elements.status.textContent, 'Preparando…');
  assert.equal(harness.elements.go.textContent, 'Go');
});

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

test('host commands wait for the app launch, so init_ui cannot wipe them', async () => {
  const harness = createHarness({ ready: false });
  const launch = deferred();
  const wasm = {
    ...harness.wasm,
    // gpui-web returns from `run` before the launch callback; the launch
    // callback runs init_ui, which resets the language and overrides.
    run() {
      harness.calls.push(['run']);
      return launch.promise.then(() => {
        harness.calls.push(['init_ui']);
        harness.catalogState.language = 'en';
        harness.catalogState.overrides = {};
      });
    },
  };
  const overrides = { browser: { boot: { go_button: leaf('Continuar') } } };
  const launching = harness.launch(wasm, new Uint8Array(0), null);
  await Promise.resolve();
  harness.send({ ...command('set_language', 'lang-1'), language: 'es' });
  harness.send({ ...command('replace_overrides', 'cat-1'), overrides });

  assert.equal(harness.state().wasmReady, false);
  assert.deepEqual(harness.posted, [], 'nothing is acknowledged before launch');
  assert.throws(() => harness.api.setLanguage('es'), /not ready/);

  launch.resolve();
  await launching;

  assert.deepEqual(
    harness.calls.map(([name]) => name),
    ['run', 'init_ui', 'set_language', 'replace_translation_overrides'],
  );
  assert.equal(harness.catalogState.language, 'es');
  assert.deepEqual(harness.catalogState.overrides, overrides);
  assert.equal(harness.elements.go.textContent, 'Continuar');
  assert.deepEqual(
    harness.posted.map(({ data }) => [data.requestId ?? data.type, data.ok ?? null]),
    [
      ['lang-1', true],
      ['cat-1', true],
      ['ready', null],
    ],
  );
});

test('a launch failure rejects queued commands and never reports ready', async () => {
  const harness = createHarness({ ready: false });
  const launch = deferred();
  const wasm = { ...harness.wasm, run: () => launch.promise };
  const launching = harness.launch(wasm, new Uint8Array(0), null);
  harness.send({ ...command('set_language', 'lang-1'), language: 'es' });
  launch.reject(new Error('SQLLY could not start: browser graphics are unavailable'));
  await assert.rejects(launching, /graphics are unavailable/);

  assert.equal(harness.state().wasmReady, false);
  assert.deepEqual(harness.calls, []);
  assert.deepEqual(plain(harness.posted.map(({ data }) => data)), [
    {
      channel: CHANNEL,
      requestId: 'lang-1',
      type: 'ack',
      requestType: 'set_language',
      ok: false,
      error: 'SQLLY could not start: browser graphics are unavailable',
    },
  ]);
  assert.throws(() => harness.api.setLanguage('es'), /not ready/);
});

test('size check counts UTF-8 bytes for multi-byte payloads near the limit', () => {
  const harness = createHarness();
  // 'é' is two UTF-8 bytes: the string is under the limit in code units but
  // over it in bytes, so it must take the encoding path and be rejected.
  const overrides = { browser: { boot: { preparing: leaf('') } } };
  const emptySize = Buffer.byteLength(JSON.stringify(overrides));
  overrides.browser.boot.preparing.value = 'é'.repeat(Math.ceil((MAX_TRANSLATION_BYTES - emptySize) / 2) + 1);
  assert(JSON.stringify(overrides).length < MAX_TRANSLATION_BYTES);
  assert.throws(() => harness.api.replaceOverrides(overrides), /8 MiB/);
  assert.deepEqual(harness.calls, []);
});
