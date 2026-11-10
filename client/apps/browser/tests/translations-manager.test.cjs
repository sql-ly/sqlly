// Tests for the /translations manager page script, which lives with the
// website (server/Sqlly.Api/wwwroot/assets/pages/translations.js) but speaks
// the same postMessage protocol as www/boot.js. The script runs unmodified in
// a vm context against a small fake DOM, fetch, and timer set.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const ORIGIN = 'https://sqlly.test';
const CHANNEL = 'sqlly.translations.v1';
const TOKEN = 'antiforgery-token-value';
const namespaces = require('../../../../server/Sqlly.Api/wwwroot/assets/pages/translation-namespaces.js');
const managerSource = fs.readFileSync(
  path.join(__dirname, '../../../../server/Sqlly.Api/wwwroot/assets/pages/translations.js'),
  'utf8',
);

function element(id) {
  const listeners = new Map();
  const node = {
    id,
    hidden: false,
    disabled: false,
    value: '',
    textContent: '',
    dataset: {},
    attributes: {},
    children: [],
    span: null,
    focus() { this.focused = true; },
    setAttribute(name, value) {
      this.attributes[name] = value;
    },
    addEventListener(type, listener) {
      if (!listeners.has(type)) listeners.set(type, []);
      listeners.get(type).push(listener);
    },
    dispatch(type, event = {}) {
      (listeners.get(type) || []).forEach((listener) => listener(event));
    },
    listenerCount(type) {
      return (listeners.get(type) || []).length;
    },
    replaceChildren() {
      this.children = [];
    },
    append(...children) {
      this.children.push(...children);
    },
    appendChild(child) {
      this.children.push(child);
    },
    querySelector(selector) {
      if (selector === 'span') return this.span || (this.span = element());
      return null;
    },
    querySelectorAll(selector) {
      if (selector !== '[data-namespace]') return [];
      const found = [];
      const visit = (node) => node.children.forEach((child) => {
        if (child.dataset && child.dataset.namespace !== undefined) found.push(child);
        if (child.children) visit(child);
      });
      visit(this);
      return found;
    },
    removeAttribute(name) {
      delete this.attributes[name];
    },
  };
  return node;
}

function entry(key, value, extra = {}) {
  return {
    key,
    value,
    originalValue: value,
    location: `Location of ${key}`,
    isOverride: false,
    canUndo: false,
    votes: { upVotes: 0, downVotes: 0, myVote: 0 },
    ...extra,
  };
}

// The load response: entries only; the page derives the preview catalog.
function snapshot(entries, language = 'es') {
  return { language, entries };
}

// A save/restore/undo/vote response: only the changed entry.
function patch(changed, language = 'es') {
  return { language, entry: changed };
}

// A quarantined override exactly as the server sends it: `value` is the
// persisted wording (the expected-current token), the app shows the original.
function quarantined(key, value, originalValue) {
  return entry(key, value, { originalValue, isOverride: true, applied: false, stale: 'placeholders' });
}

test('stale wording is flagged and history is paginated without changing drafts', async () => {
  const harness = createHarness();
  harness.respond(200, snapshot([quarantined('settings.title', 'Old {count}', 'New {total}')]));
  harness.start();
  await settleAll();
  assert.equal(harness.elements.translationStale.hidden, false);
  assert.equal(harness.elements.translationStale.textContent, 'This wording no longer matches the current catalog. The preview uses the original until you repair or restore it.');
  harness.elements.translationValue.value = 'Draft {total}';
  harness.respond(200, { entries: [{ sequence: 26, actor: 'Tr…', at: '2026-10-26T12:00:00Z', newValue: 'Old {count}', notes: 'Note' }], nextBefore: 26 });
  harness.elements.loadTranslationHistory.dispatch('click');
  await settleAll();
  assert.equal(harness.elements.translationHistory.children.length, 1);
  harness.respond(200, { entries: [], nextBefore: null });
  harness.elements.loadTranslationHistory.dispatch('click');
  await settleAll();
  assert.match(harness.requests.at(-1).url, /before=26$/);
  assert.equal(harness.elements.loadTranslationHistory.hidden, true);
  assert.equal(harness.elements.translationValue.value, 'Draft {total}');
});

function createHarness() {
  const ids = [
    'translationLanguage', 'translationSearch', 'translationResults', 'translationResultCount',
    'translationEditor', 'translationKey', 'translationLocation', 'translationOriginal',
    'translationValue', 'translationNotes', 'saveTranslation', 'restoreTranslation',
    'undoTranslation', 'upvoteTranslation', 'downvoteTranslation', 'translationStatus',
    'translationPreview', 'setTranslationLanguage', 'translationStale', 'translationHistory', 'loadTranslationHistory',
    'translationHistoryEmpty', 'discardTranslation', 'translationDraftState', 'translationDraftCount',
    'translationPreviewStatus', 'retryTranslationPreview', 'translationEmpty', 'translationHistoryDetails',
    'translationSaveControls', 'translationVoteSummary', 'translationReview', 'clearTranslationSearch',
    'showTranslationEditor', 'showTranslationPreview',
    'translationSurface', 'translationContext', 'translationRejected', 'translationLanguageHint',
    'translationNamespacePicker', 'translationNamespaceSummary', 'translationNamespaceTree', 'showAllTranslationNamespaces',
  ];
  const elements = Object.fromEntries(ids.map((id) => [id, element(id)]));
  const root = element('translationManager');
  const messages = JSON.parse(fs.readFileSync(path.join(__dirname, '../../../crates/sqlly-i18n/translations/en.json'), 'utf8')).translation_manager;
  root.dataset.messages = JSON.stringify(Object.fromEntries(Object.entries(messages).map(([key, leaf]) => ['translation_manager.' + key, leaf.value])));
  root.querySelector = (selector) =>
    selector === 'input[name="__RequestVerificationToken"]' ? { value: TOKEN } : null;
  elements.translationManager = root;
  elements.translationLanguage.value = 'es';
  elements.translationLanguage.options = [{ text: 'Español' }];
  elements.translationLanguage.selectedIndex = 0;

  const posted = [];
  const contentWindow = {
    postMessage(data, targetOrigin) {
      posted.push({ data: JSON.parse(JSON.stringify(data)), targetOrigin });
    },
  };
  elements.translationPreview.contentWindow = contentWindow;
  let reloads = 0;
  let src = '/app/';
  Object.defineProperty(elements.translationPreview, 'src', {
    get: () => src,
    set: (value) => { src = value; reloads += 1; },
  });

  const requests = [];
  const responses = [];
  const timers = [];
  const windowListeners = new Map();
  const context = {
    document: {
      getElementById: (id) => elements[id] || null,
      createElement: () => element(),
    },
    location: { origin: ORIGIN },
    crypto: { randomUUID: (() => { let n = 0; return () => `uuid-${++n}`; })() },
    fetch(url, options) {
      requests.push({ url, options });
      const next = responses.shift();
      if (!next) throw new Error(`unexpected fetch ${url}`);
      return Promise.resolve({
        ok: next.status >= 200 && next.status < 300,
        status: next.status,
        json: () => Promise.resolve(next.body),
      });
    },
    setTimeout(callback, delay) {
      const timer = { callback, delay, cleared: false };
      timers.push(timer);
      return timer;
    },
    clearTimeout(timer) {
      if (timer) timer.cleared = true;
    },
    addEventListener(type, listener) {
      windowListeners.set(type, listener);
    },
    TranslationNamespaces: namespaces,
    Error,
    JSON,
    Array,
    Promise,
    Object,
    Date,
    Math,
  };
  context.window = context;

  return {
    elements,
    posted,
    requests,
    timers,
    respond(status, body) {
      responses.push({ status, body });
    },
    start() {
      vm.createContext(context);
      vm.runInContext(managerSource, context);
    },
    fromPreview(data, { origin = ORIGIN, source = contentWindow } = {}) {
      windowListeners.get('message')({ data, origin, source });
    },
    ready() {
      this.fromPreview({ channel: CHANNEL, type: 'ready' });
    },
    ack(index, extra = {}) {
      const command = posted[index].data;
      this.fromPreview({ channel: CHANNEL, type: 'ack', requestId: command.requestId, requestType: command.type, ok: true, ...extra });
    },
    beforeUnload(event) { windowListeners.get('beforeunload')(event); },
    reloads() { return reloads; },
    runTimers() {
      timers.splice(0).filter((timer) => !timer.cleared).forEach((timer) => timer.callback());
    },
    status() {
      return { text: elements.translationStatus.textContent, kind: elements.translationStatus.dataset.kind };
    },
  };
}

const settle = () => new Promise((resolve) => setImmediate(resolve));
async function settleAll() {
  for (let index = 0; index < 5; index += 1) await settle();
}

async function started(entries = [entry('settings.title', 'Ajustes'), entry('menu.file', 'Archivo')]) {
  const harness = createHarness();
  harness.respond(200, snapshot(entries));
  harness.start();
  await settleAll();
  return harness;
}

test('the manager pushes only after the preview reports ready, never on iframe load', async () => {
  const harness = await started();
  assert.equal(harness.elements.translationPreview.listenerCount('load'), 0);
  harness.elements.translationPreview.dispatch('load');
  assert.deepEqual(harness.posted, [], 'no push before ready');

  harness.ready();
  assert.deepEqual(
    harness.posted.map(({ data, targetOrigin }) => [data.type, data.channel, targetOrigin]),
    [
      ['set_language', CHANNEL, ORIGIN],
      ['replace_overrides', CHANNEL, ORIGIN],
    ],
  );
  assert.equal(harness.posted[0].data.language, 'es');
  assert.deepEqual(harness.posted[1].data.overrides, {
    settings: { title: { value: 'Ajustes', location: 'Location of settings.title' } },
    menu: { file: { value: 'Archivo', location: 'Location of menu.file' } },
  });
  assert.notEqual(harness.posted[0].data.requestId, harness.posted[1].data.requestId);
  assert.equal(harness.elements.translationPreviewStatus.textContent, 'Preview updating…');
  harness.ack(1);
  assert.equal(harness.elements.translationPreviewStatus.textContent, 'Preview updating…');
  harness.ack(0);
  assert.equal(harness.elements.translationPreviewStatus.textContent, 'Preview updated.');
  assert.deepEqual(harness.status(), { text: 'Translations loaded.', kind: 'success' });
});

test('a second ready (iframe reload) re-pushes the latest pair', async () => {
  const harness = await started();
  harness.ready();
  harness.posted.length = 0;
  harness.ready();
  assert.deepEqual(harness.posted.map(({ data }) => data.type), ['set_language', 'replace_overrides']);
});

test('ready messages from other origins or windows are ignored', async () => {
  const harness = await started();
  harness.fromPreview({ channel: CHANNEL, type: 'ready' }, { origin: 'https://attacker.test' });
  harness.fromPreview({ channel: CHANNEL, type: 'ready' }, { source: {} });
  assert.deepEqual(harness.posted, []);
});

test('mutations send the antiforgery header and expectedCurrentValue', async () => {
  const harness = await started();
  harness.elements.translationValue.value = 'Configuración';
  harness.elements.translationNotes.value = 'clearer';
  harness.respond(200, patch(entry('settings.title', 'Configuración', { isOverride: true, canUndo: true })));
  harness.elements.saveTranslation.dispatch('click');
  await settleAll();

  const save = harness.requests[1];
  assert.equal(save.url, '/api/translations/es/save');
  assert.equal(save.options.method, 'POST');
  assert.equal(save.options.headers.RequestVerificationToken, TOKEN);
  assert.equal(save.options.headers['Content-Type'], 'application/json');
  assert.deepEqual(JSON.parse(save.options.body), {
    key: 'settings.title',
    expectedCurrentValue: 'Ajustes',
    notes: 'clearer',
    value: 'Configuración',
  });

  assert.equal(harness.elements.translationValue.value, 'Configuración');
  assert.equal(harness.elements.translationResults.children[0].children[0].textContent, 'Configuración');
  harness.respond(200, patch(entry('settings.title', 'Ajustes')));
  harness.elements.restoreTranslation.dispatch('click');
  await settleAll();
  const restore = harness.requests[2];
  assert.equal(restore.url, '/api/translations/es/restore-original');
  assert.equal(restore.options.headers.RequestVerificationToken, TOKEN);
  assert.equal(JSON.parse(restore.options.body).expectedCurrentValue, 'Configuración');
  assert.equal('value' in JSON.parse(restore.options.body), false);
  assert.equal(harness.elements.translationValue.value, 'Ajustes');
  assert.equal(harness.elements.restoreTranslation.disabled, true);
});

test('a failed save keeps the translator draft; a successful save shows the saved value', async () => {
  const harness = await started();
  harness.elements.translationValue.value = 'My careful draft';
  harness.respond(409, { error: 'Someone else changed this wording.' });
  harness.elements.saveTranslation.dispatch('click');
  await settleAll();

  assert.equal(harness.elements.translationValue.value, 'My careful draft');
  assert.deepEqual(harness.status(), { text: 'Someone else changed this wording.', kind: 'error' });
  assert.equal(harness.elements.saveTranslation.disabled, false);

  harness.respond(200, patch(entry('settings.title', 'Server value')));
  harness.elements.saveTranslation.dispatch('click');
  await settleAll();
  assert.equal(harness.elements.translationValue.value, 'Server value');
  assert.equal(harness.status().kind, 'success');
});

test('successful mutations push the new catalog to a ready preview', async () => {
  const harness = await started();
  harness.ready();
  harness.posted.length = 0;
  harness.elements.translationValue.value = 'Nuevo';
  harness.respond(200, patch(entry('settings.title', 'Nuevo', { isOverride: true })));
  harness.elements.saveTranslation.dispatch('click');
  await settleAll();
  assert.deepEqual(harness.posted.map(({ data }) => data.type), ['set_language', 'replace_overrides']);
  assert.equal(harness.posted[1].data.overrides.settings.title.value, 'Nuevo');
  assert.equal(harness.posted[1].data.overrides.menu.file.value, 'Archivo');
});

test('vote updates do not clobber the draft', async () => {
  const harness = await started();
  harness.elements.translationValue.value = 'Unsaved draft';
  harness.respond(200, patch(entry('settings.title', 'Ajustes', { votes: { upVotes: 1234, downVotes: 1, myVote: 1 } })));
  harness.elements.upvoteTranslation.dispatch('click');
  await settleAll();

  const vote = harness.requests[1];
  assert.equal(vote.url, '/api/translations/es/vote');
  assert.equal(vote.options.headers.RequestVerificationToken, TOKEN);
  assert.deepEqual(JSON.parse(vote.options.body), { key: 'settings.title', expectedCurrentValue: 'Ajustes', value: 1 });
  assert.equal(harness.elements.translationValue.value, 'Unsaved draft');
  assert.equal(harness.elements.upvoteTranslation.span.textContent, `Upvote (${new Intl.NumberFormat().format(1234)})`);
  assert.equal(harness.elements.upvoteTranslation.attributes['aria-pressed'], 'true');

  harness.respond(500, { error: 'Vote failed.' });
  harness.elements.downvoteTranslation.dispatch('click');
  await settleAll();
  assert.equal(harness.elements.translationValue.value, 'Unsaved draft');
});

test('an ack with skipped keys shows a non-blocking notice, not an error', async () => {
  const harness = await started();
  harness.ready();
  harness.ack(0);
  harness.ack(1, { skipped: ['a.b', 'c.d'] });
  assert.equal(harness.elements.translationPreviewStatus.dataset.kind, 'notice');
  assert.match(harness.elements.translationPreviewStatus.textContent, /2 entries skipped/);
  assert.deepEqual(harness.status(), { text: 'Translations loaded.', kind: 'success' });
  harness.ready();
  harness.ack(2, { ok: false, error: 'boom' });
  assert.match(harness.elements.translationPreviewStatus.textContent, /boom/);
  harness.ack(3);
  assert.equal(harness.elements.translationPreviewStatus.dataset.kind, 'error', 'late acknowledgements never hide a failure');
});

test('search input is debounced', async () => {
  const harness = await started([entry('settings.title', 'Ajustes'), entry('menu.file', 'Archivo'), entry('menu.edit', 'Editar')]);
  assert.equal(harness.elements.translationResults.children.length, 3);
  harness.timers.length = 0;
  const search = harness.elements.translationSearch;
  search.value = 'a';
  search.dispatch('input');
  search.value = 'archivo';
  search.dispatch('input');
  assert.equal(harness.elements.translationResults.children.length, 3, 'no filtering until the debounce fires');
  assert.equal(harness.timers.length, 2);
  assert.equal(harness.timers[0].cleared, true);
  assert.equal(harness.timers[1].delay, 120);
  harness.runTimers();
  assert.equal(harness.elements.translationResults.children.length, 1);
  assert.equal(harness.elements.translationResultCount.textContent, 'Matching translations: 1');
});

test('out-of-order loads cannot change the language of the next save', async () => {
  const h = await started();
  let finish;
  h.respond(200, new Promise(resolve => { finish = resolve; }));
  h.elements.translationLanguage.value = 'en';
  h.elements.setTranslationLanguage.dispatch('click');
  h.respond(200, snapshot([entry('settings.title', 'Español')]));
  h.elements.translationLanguage.value = 'es';
  h.elements.setTranslationLanguage.dispatch('click');
  await settleAll();
  finish(snapshot([entry('settings.title', 'English')], 'en'));
  await settleAll();
  h.respond(200, patch(entry('settings.title', 'Guardado')));
  h.elements.translationValue.value = 'Guardado';
  h.elements.saveTranslation.dispatch('click');
  await settleAll();
  assert.equal(h.requests.at(-1).url, '/api/translations/es/save');
  assert.equal(h.elements.translationLanguage.value, 'es');
});

for (const switchEntry of [false, true]) {
  test(`save completion preserves later edits (switch entry: ${switchEntry})`, async () => {
    const h = await started();
    let finish;
    h.elements.translationValue.value = 'Submitted';
    h.respond(200, new Promise(resolve => { finish = resolve; }));
    h.elements.saveTranslation.dispatch('click');
    if (switchEntry) h.elements.translationResults.children[1].dispatch('click');
    h.elements.translationValue.value = 'Later draft';
    h.elements.translationNotes.value = 'Later note';
    finish(patch(entry('settings.title', 'Submitted')));
    await settleAll();
    assert.equal(h.elements.translationValue.value, 'Later draft');
    assert.equal(h.elements.translationNotes.value, 'Later note');
  });
}

test('language loads retain edits made while the request is pending', async () => {
  const h = await started();
  let finish;
  h.elements.translationLanguage.value = 'en';
  h.respond(200, new Promise(resolve => { finish = resolve; }));
  h.elements.setTranslationLanguage.dispatch('click');
  h.elements.translationValue.value = 'Draft while loading';
  h.elements.translationNotes.value = 'Keep this note';
  finish(snapshot([entry('settings.title', 'Settings')], 'en'));
  await settleAll();
  h.elements.translationLanguage.value = 'es';
  h.respond(200, snapshot([entry('settings.title', 'Ajustes')], 'es'));
  h.elements.setTranslationLanguage.dispatch('click');
  await settleAll();
  assert.equal(h.elements.translationValue.value, 'Draft while loading');
  assert.equal(h.elements.translationNotes.value, 'Keep this note');
});

test('results lead with wording, have one tab stop and keep votes in the editor', async () => {
  const h = await started();
  assert.deepEqual(h.elements.translationResults.children.map(e => e.tabIndex), [0, -1]);
  h.elements.translationResults.children[0].dispatch('keydown', { key: 'ArrowDown', preventDefault() {} });
  assert.equal(h.elements.translationKey.textContent, 'menu.file');
  assert.equal(h.elements.translationResults.children[1].focused, true);
  assert.equal(h.elements.translationResults.children[1].children[0].textContent, 'Archivo');
  assert.equal(h.elements.translationResults.children[1].children.length, 3);
  assert.match(h.elements.translationVoteSummary.textContent, /Your vote: 0/);
});

test('draft indicators, no-op saves, discard and navigation protection follow actual changes', async () => {
  const h = await started();
  const e = h.elements;
  assert.equal(e.saveTranslation.disabled, true);
  assert.equal(e.discardTranslation.disabled, true);
  e.saveTranslation.dispatch('click');
  assert.equal(h.requests.length, 1, 'unchanged wording is not saved');
  e.translationValue.value = 'Draft';
  e.translationValue.dispatch('input');
  assert.equal(e.saveTranslation.disabled, false);
  assert.equal(e.translationDraftState.textContent, 'Unsaved changes');
  assert.equal(e.translationResults.children[0].children[2].hidden, false);
  assert.match(e.translationDraftCount.textContent, /Unsaved drafts: 1/);
  let prevented = false;
  h.beforeUnload({ preventDefault() { prevented = true; } });
  assert.equal(prevented, true);
  e.translationResults.children[1].dispatch('click');
  assert.equal(e.saveTranslation.disabled, true);
  e.translationResults.children[0].dispatch('click');
  assert.equal(e.translationValue.value, 'Draft');
  e.discardTranslation.dispatch('click');
  assert.equal(e.translationValue.value, 'Ajustes');
  assert.equal(e.translationDraftCount.hidden, true);
  prevented = false;
  h.beforeUnload({ preventDefault() { prevented = true; } });
  assert.equal(prevented, false);
  e.translationNotes.value = 'Only a note';
  e.translationNotes.dispatch('input');
  assert.equal(e.discardTranslation.disabled, false);
  assert.equal(e.saveTranslation.disabled, false, 'a note alone can be saved as a review revision');
  h.respond(200, patch(entry('settings.title', 'Ajustes')));
  e.saveTranslation.dispatch('click');
  await settleAll();
  assert.deepEqual(JSON.parse(h.requests.at(-1).options.body), { key: 'settings.title', expectedCurrentValue: 'Ajustes', notes: 'Only a note', value: 'Ajustes' });
  assert.equal(e.translationDraftCount.hidden, true);
  assert.equal(e.translationNotes.value, '');
});

test('save confirmation is independent of preview readiness, correlated errors and retries', async () => {
  const h = await started();
  h.elements.translationValue.value = 'Saved wording';
  h.respond(200, patch(entry('settings.title', 'Saved wording')));
  h.elements.saveTranslation.dispatch('click');
  await settleAll();
  assert.deepEqual(h.status(), { text: 'Saved.', kind: 'success' });
  assert.match(h.elements.translationPreviewStatus.textContent, /Preview connecting/);
  h.ready();
  h.fromPreview({ channel: CHANNEL, type: 'ack', requestId: 'unknown', requestType: 'set_language', ok: false, error: 'stale' });
  assert.equal(h.elements.translationPreviewStatus.textContent, 'Preview updating…');
  h.ack(0, { requestType: 'replace_overrides' });
  h.ack(1);
  assert.equal(h.elements.translationPreviewStatus.textContent, 'Preview updating…', 'mismatched type is ignored');
  h.runTimers();
  assert.equal(h.elements.translationPreviewStatus.dataset.kind, 'error');
  assert.equal(h.elements.retryTranslationPreview.hidden, false);
  assert.equal(h.status().text, 'Saved.');
  // A timed-out preview is reloaded and the pair is re-sent on its next ready.
  h.elements.retryTranslationPreview.dispatch('click');
  assert.equal(h.reloads(), 1);
  assert.equal(h.posted.length, 2, 'nothing is sent to a reloading preview');
  h.ack(0, { ok: false, error: 'late failure' });
  h.ready();
  h.ack(2);
  h.ack(3);
  assert.equal(h.elements.translationPreviewStatus.textContent, 'Preview updated.');
  assert.equal(h.status().text, 'Saved.');
});

test('a late acknowledgement after a timeout still reports the preview as updated', async () => {
  const h = await started();
  h.ready();
  h.runTimers();
  assert.equal(h.elements.translationPreviewStatus.dataset.kind, 'error');
  h.ack(0);
  h.ack(1);
  assert.equal(h.elements.translationPreviewStatus.textContent, 'Preview updated.');
  assert.equal(h.elements.retryTranslationPreview.hidden, true);
});

test('retry without a timeout re-sends to a ready preview without reloading it', async () => {
  const h = await started();
  h.ready();
  h.fromPreview({ channel: CHANNEL, type: 'ack', requestId: h.posted[0].data.requestId, requestType: 'set_language', ok: false, error: 'boom' });
  h.elements.retryTranslationPreview.dispatch('click');
  assert.equal(h.reloads(), 0);
  assert.deepEqual(h.posted.slice(2).map(({ data }) => data.type), ['set_language', 'replace_overrides']);
});

test('empty search has recovery, view switching preserves the editor and blank wording cannot save', async () => {
  const h = await started();
  const e = h.elements;
  e.translationValue.value = '  ';
  e.translationValue.dispatch('input');
  assert.equal(e.saveTranslation.disabled, true);
  e.showTranslationPreview.dispatch('click');
  assert.equal(e.translationManager.dataset.view, 'preview');
  assert.equal(e.showTranslationPreview.attributes['aria-pressed'], 'true');
  e.showTranslationEditor.dispatch('click');
  assert.equal(e.translationValue.value, '  ');
  e.translationSearch.value = 'no match whatsoever';
  e.translationSearch.dispatch('input');
  h.runTimers();
  assert.equal(e.translationEmpty.hidden, false);
  e.clearTranslationSearch.dispatch('click');
  assert.equal(e.translationEmpty.hidden, true);
  assert.equal(e.translationResults.children.length, 2);
});

test('quarantined wording must be repaired before saving and a completed save clears navigation protection', async () => {
  const h = createHarness();
  h.respond(200, snapshot([quarantined('settings.title', 'Recovered {count}', 'Built-in {total}')]));
  h.start();
  await settleAll();
  // The editor shows the persisted wording; unchanged, it would fail placeholder validation.
  assert.equal(h.elements.translationValue.value, 'Recovered {count}');
  assert.equal(h.elements.saveTranslation.disabled, true);
  assert.equal(h.elements.translationStale.hidden, false);
  h.ready();
  assert.equal(h.posted[1].data.overrides.settings.title.value, 'Built-in {total}', 'the preview shows the original');
  h.elements.translationValue.value = 'Recovered {total}';
  h.elements.translationValue.dispatch('input');
  assert.equal(h.elements.saveTranslation.disabled, false);
  h.respond(200, patch(entry('settings.title', 'Recovered {total}', { originalValue: 'Built-in {total}', isOverride: true, canUndo: true })));
  h.elements.saveTranslation.dispatch('click');
  await settleAll();
  assert.equal(JSON.parse(h.requests.at(-1).options.body).expectedCurrentValue, 'Recovered {count}');
  assert.equal(h.elements.translationStale.hidden, true);
  assert.equal(h.elements.translationDraftCount.hidden, true);
  assert.equal(h.elements.saveTranslation.disabled, true);
  assert.equal(h.posted.at(-1).data.overrides.settings.title.value, 'Recovered {total}');
  let prevented = false;
  h.beforeUnload({ preventDefault() { prevented = true; } });
  assert.equal(prevented, false);
});

test('an unready preview times out with recovery without changing the save status', async () => {
  const h = await started();
  h.runTimers();
  assert.equal(h.elements.retryTranslationPreview.hidden, false);
  assert.equal(h.elements.translationPreviewStatus.dataset.kind, 'error');
  assert.equal(h.status().text, 'Translations loaded.');
  h.elements.retryTranslationPreview.dispatch('click');
  assert.equal(h.elements.retryTranslationPreview.hidden, true);
  h.ready();
  h.ack(0);
  h.ack(1);
  assert.equal(h.elements.translationPreviewStatus.textContent, 'Preview updated.');
});

test('an override whose source wording changed can be re-confirmed without editing', async () => {
  const h = await started([entry('settings.title', 'Ajustes', { originalValue: 'Configuración', isOverride: true, stale: 'source' })]);
  assert.equal(h.elements.translationStale.hidden, false);
  assert.equal(h.elements.translationStale.textContent, 'The built-in wording changed after this translation was saved. Review it, then save to confirm it still fits.');
  assert.equal(h.elements.saveTranslation.disabled, false);
  h.respond(200, patch(entry('settings.title', 'Ajustes', { originalValue: 'Configuración', isOverride: true })));
  h.elements.saveTranslation.dispatch('click');
  await settleAll();
  assert.equal(JSON.parse(h.requests.at(-1).options.body).value, 'Ajustes');
  assert.equal(h.elements.translationStale.hidden, true);
  assert.equal(h.elements.saveTranslation.disabled, true);
});

test('a voted-down override is flagged and the preview falls back to the original', async () => {
  const h = await started([entry('settings.title', 'Ajustitos', { originalValue: 'Ajustes', isOverride: true, applied: false, rejected: true, votes: { upVotes: 0, downVotes: 2, myVote: -1 } })]);
  assert.equal(h.elements.translationRejected.hidden, false);
  h.ready();
  assert.equal(h.posted[1].data.overrides.settings.title.value, 'Ajustes');
  h.respond(200, patch(entry('settings.title', 'Ajustitos', { originalValue: 'Ajustes', isOverride: true, votes: { upVotes: 3, downVotes: 2, myVote: 1 } })));
  h.elements.upvoteTranslation.dispatch('click');
  await settleAll();
  assert.equal(h.elements.translationRejected.hidden, true);
  assert.equal(h.posted.at(-1).data.overrides.settings.title.value, 'Ajustitos');
});

test('selecting an entry or moving with arrow keys keeps the rendered results', async () => {
  const h = await started([entry('settings.title', 'Ajustes'), entry('menu.file', 'Archivo'), entry('menu.edit', 'Editar')]);
  const buttons = h.elements.translationResults.children.slice();
  buttons[2].dispatch('click');
  assert.deepEqual(h.elements.translationResults.children, buttons, 'no rebuild on selection');
  assert.deepEqual(buttons.map((button) => button.attributes['aria-selected']), ['false', 'false', 'true']);
  assert.deepEqual(buttons.map((button) => button.tabIndex), [-1, -1, 0]);
  buttons[2].dispatch('keydown', { key: 'Home', preventDefault() {} });
  assert.deepEqual(h.elements.translationResults.children, buttons);
  assert.equal(buttons[0].focused, true);
  assert.equal(h.elements.translationKey.textContent, 'settings.title');
  assert.deepEqual(buttons.map((button) => button.tabIndex), [0, -1, -1]);
});

test('a save superseded by Set language does not leave its draft behind', async () => {
  const h = await started();
  let finish;
  h.elements.translationValue.value = 'Guardado';
  h.elements.translationNotes.value = 'with a note';
  h.respond(200, new Promise((resolve) => { finish = resolve; }));
  h.elements.saveTranslation.dispatch('click');
  h.respond(200, snapshot([entry('settings.title', 'Guardado', { isOverride: true }), entry('menu.file', 'Archivo')]));
  h.elements.setTranslationLanguage.dispatch('click');
  await settleAll();
  finish(patch(entry('settings.title', 'Guardado', { isOverride: true })));
  await settleAll();
  assert.equal(h.elements.translationDraftCount.hidden, true);
  assert.equal(h.elements.translationValue.value, 'Guardado');
  assert.equal(h.elements.translationNotes.value, '');
  let prevented = false;
  h.beforeUnload({ preventDefault() { prevented = true; } });
  assert.equal(prevented, false);
});

test('choosing another language without Set language explains why editing paused', async () => {
  const h = await started();
  assert.equal(h.elements.translationLanguageHint.hidden, true);
  h.elements.translationLanguage.value = 'en';
  h.elements.translationLanguage.options = [{ text: 'English' }];
  h.elements.translationLanguage.dispatch('change');
  assert.equal(h.elements.translationLanguageHint.hidden, false);
  assert.equal(h.elements.translationLanguageHint.textContent, 'Choose Set language to edit English. Editing stays paused until then.');
  assert.equal(h.elements.restoreTranslation.disabled, true);
  h.elements.translationLanguage.value = 'es';
  h.elements.translationLanguage.dispatch('change');
  assert.equal(h.elements.translationLanguageHint.hidden, true);
});

test('after a language load the auto-selected entry is one the filters show', async () => {
  const h = await started();
  h.elements.translationSearch.value = 'archivo';
  h.elements.translationSearch.dispatch('input');
  h.runTimers();
  h.elements.translationLanguage.value = 'en';
  h.respond(200, snapshot([entry('settings.title', 'Settings'), entry('menu.file', 'Archivo en')], 'en'));
  h.elements.setTranslationLanguage.dispatch('click');
  await settleAll();
  assert.equal(h.elements.translationKey.textContent, 'menu.file');
  assert.deepEqual(h.elements.translationResults.children.map((button) => button.attributes['aria-selected']), ['true']);
});

test('history timestamps and vote counts are formatted for the page language', async () => {
  const h = await started();
  const at = '2026-10-26T12:00:00Z';
  h.respond(200, { entries: [{ sequence: 1, actor: 'Ana', at, newValue: 'Ajustes', notes: null }], nextBefore: null });
  h.elements.loadTranslationHistory.dispatch('click');
  await settleAll();
  const heading = h.elements.translationHistory.children[0].children[0].textContent;
  assert.ok(!heading.includes(at), heading);
  assert.ok(heading.includes(new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(at))), heading);
  assert.match(heading, /Ana/);
});
