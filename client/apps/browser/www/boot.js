// Boot script for the SQLLY browser preview (in-tab demo).
//
// Kept as an EXTERNAL module (not inline in index.html) so the page loads
// under the marketing site's strict Content-Security-Policy
// (`script-src 'self' 'nonce-…' 'wasm-unsafe-eval'`) with no per-path
// exemption — the same pattern the sqlly-datatable demo uses. An inline
// <script> would need a nonce we cannot stamp into a statically-served file.
const loadingEl = document.getElementById('loading');
const progressEl = document.getElementById('progress');
const statusEl = document.getElementById('status');
const factEl = document.getElementById('fact');

const TRANSLATION_CHANNEL = 'sqlly.translations.v1';
const MAX_TRANSLATION_BYTES = 8 * 1024 * 1024;
const KEY_SEGMENT = /^[a-z][a-z0-9]*(?:_[a-z0-9]+)*$/;
const SUPPORTED_LANGUAGES = ['en', 'es', 'zh-CN', 'ja', 'pt-BR', 'ko', 'de', 'ru', 'fr', 'zh-TW'];
const utf8 = new TextEncoder();
let wasmModule = null;
let wasmReady = false;
let activeLanguage = 'en';
let statusState = { key: 'browser.boot.preparing', fallback: 'Preparing…', arguments: {} };

/* A rejection the translation manager can show in the translator's language.
   `message` stays the English text (older hosts show it); `code` names the
   translation_manager.preview_error_{code} catalog entry and `params` fill its
   named placeholders (strings only). */
class TranslationError extends Error {
  constructor(code, message, params = {}) {
    super(message);
    this.code = code;
    this.params = params;
  }
}

function isPlainObject(value) {
  if (Object.prototype.toString.call(value) !== '[object Object]') {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  if (prototype === null) {
    return true;
  }
  const constructor = Object.prototype.hasOwnProperty.call(prototype, 'constructor')
    ? prototype.constructor
    : null;
  return (
    typeof constructor === 'function' &&
    Function.prototype.toString.call(constructor) === Function.prototype.toString.call(Object)
  );
}

function ownKeys(value) {
  return Reflect.ownKeys(value);
}

function hasExactFields(value, fields) {
  const keys = ownKeys(value);
  return keys.length === fields.length && fields.every((field) => keys.includes(field));
}

function validateLanguage(language) {
  if (!SUPPORTED_LANGUAGES.includes(language)) {
    throw new TranslationError('language_invalid', `Language must be one of: ${SUPPORTED_LANGUAGES.join(', ')}`);
  }
}

/* UTF-8 needs one to three bytes per UTF-16 code unit, so the string length
   bounds the encoded size: only payloads in between need encoding. */
function exceedsTranslationLimit(json) {
  if (json.length > MAX_TRANSLATION_BYTES) {
    return true;
  }
  if (json.length * 3 <= MAX_TRANSLATION_BYTES) {
    return false;
  }
  return utf8.encode(json).byteLength > MAX_TRANSLATION_BYTES;
}

function prepareOverrides(overrides) {
  if (!isPlainObject(overrides)) {
    throw new TranslationError('catalog_invalid', 'Overrides must be a nested catalog object');
  }
  let json;
  try {
    json = JSON.stringify(overrides);
  } catch {
    throw new TranslationError('catalog_invalid', 'Overrides must be JSON serializable');
  }
  if (exceedsTranslationLimit(json)) {
    throw new TranslationError('payload_too_large', 'Translation payload exceeds 8 MiB');
  }

  function visit(node, prefix) {
    for (const key of ownKeys(node)) {
      if (typeof key !== 'string' || !KEY_SEGMENT.test(key)) {
        throw new TranslationError('catalog_key_invalid', `Invalid translation key segment: ${String(key)}`, { key: String(key) });
      }
      const child = node[key];
      const path = prefix ? `${prefix}.${key}` : key;
      if (!isPlainObject(child)) {
        throw new TranslationError('catalog_entry_invalid', `Translation entry must be an object: ${path}`, { path });
      }
      const keys = ownKeys(child);
      if (keys.includes('value') || keys.includes('location')) {
        if (!hasExactFields(child, ['value', 'location'])) {
          throw new TranslationError('catalog_entry_invalid', `Translation leaf must contain exactly value and location: ${path}`, { path });
        }
        if (typeof child.value !== 'string' || !child.value.trim()) {
          throw new TranslationError('catalog_value_invalid', `Translation value must be a nonempty string: ${path}`, { path });
        }
        if (typeof child.location !== 'string' || !child.location.trim()) {
          throw new TranslationError('catalog_location_invalid', `Translation location must be a nonempty string: ${path}`, { path });
        }
      } else {
        if (keys.length === 0) {
          throw new TranslationError('catalog_entry_invalid', `Translation group must not be empty: ${path}`, { path });
        }
        visit(child, path);
      }
    }
  }

  visit(overrides, '');
  return { json };
}

function message(key, fallback, arguments_ = {}) {
  if (wasmReady && typeof wasmModule?.translation_message === 'function') {
    return wasmModule.translation_message(key, JSON.stringify(Object.fromEntries(
      Object.entries(arguments_).map(([name, value]) => [name, String(value)]))));
  }
  // Before launch only the compiled English fallbacks exist on this side;
  // host commands are queued until then, so there is nothing to merge.
  let value = fallback;
  for (const [name, argument] of Object.entries(arguments_)) {
    value = value.replaceAll(`{${name}}`, String(argument));
  }
  return value;
}

function errorText(error) {
  return String((error && error.message) || error);
}

/* The acknowledgement fields for a failure: the English `error`, plus the
   stable `code` and `params` when the failure has them. */
function failure(error) {
  const result = { ok: false, error: errorText(error) };
  if (error instanceof TranslationError) {
    result.code = error.code;
    result.params = error.params;
  }
  return result;
}

function requireReady() {
  if (!wasmModule || !wasmReady) {
    throw new TranslationError('not_ready', 'SQLLY is not ready');
  }
}

function setLanguage(language) {
  validateLanguage(language);
  requireReady();
  applyLanguage(language);
}

function applyLanguage(language) {
  wasmModule.set_language(language);
  activeLanguage = language;
  document.documentElement.lang = language;
  refreshBootText();
}

/* The WASM host may return nothing (older builds, which reject unknown keys
   by throwing), an array of skipped dotted keys, a JSON string of that array,
   or an object with a `skipped` array (newer builds skip keys this build does
   not know instead of rejecting the whole push). Normalize all of them. */
function normalizeSkipped(result) {
  let value = result;
  if (typeof value === 'string') {
    try {
      value = JSON.parse(value);
    } catch {
      return [];
    }
  }
  if (value && !Array.isArray(value) && typeof value === 'object' && 'skipped' in value) {
    value = value.skipped;
  }
  if (value == null || typeof value[Symbol.iterator] !== 'function' || typeof value === 'string') {
    return [];
  }
  return Array.from(value, (key) => String(key));
}

function applyPreparedOverrides(prepared) {
  const skipped = normalizeSkipped(wasmModule.replace_translation_overrides(prepared.json));
  refreshBootText();
  return { skipped };
}

function replaceOverrides(overrides) {
  const prepared = prepareOverrides(overrides);
  requireReady();
  return applyPreparedOverrides(prepared);
}

function applyClearOverrides() {
  wasmModule.clear_translation_overrides();
  refreshBootText();
}

function clearOverrides() {
  requireReady();
  applyClearOverrides();
}

globalThis.sqllyTranslations = Object.freeze({
  setLanguage,
  replaceOverrides,
  clearOverrides,
});

function acknowledge(source, requestId, requestType, result) {
  const response = { channel: TRANSLATION_CHANNEL, requestId, type: 'ack', requestType, ok: result.ok };
  if (!result.ok) {
    response.error = result.error;
    if (result.code) {
      response.code = result.code;
      response.params = result.params;
    }
  } else if (result.skipped) {
    response.skipped = result.skipped;
  }
  source.postMessage(response, location.origin);
}

/* Validate a command's shape and payload without touching WASM, returning a
   function that applies it once SQLLY is ready. Throws on invalid input so
   bad commands are rejected immediately even before WASM is ready. */
function prepareCommand(data) {
  let expectedFields;
  if (data.type === 'set_language') {
    expectedFields = ['channel', 'requestId', 'type', 'language'];
  } else if (data.type === 'replace_overrides') {
    expectedFields = ['channel', 'requestId', 'type', 'overrides'];
  } else if (data.type === 'clear_overrides') {
    expectedFields = ['channel', 'requestId', 'type'];
  } else {
    throw new TranslationError('command_unknown', 'Unknown translation command');
  }
  if (!hasExactFields(data, expectedFields)) {
    throw new TranslationError('command_shape', 'Translation command has an invalid shape');
  }
  if (data.type === 'set_language') {
    validateLanguage(data.language);
    const language = data.language;
    return { slot: 'language', apply: () => (applyLanguage(language), {}) };
  }
  if (data.type === 'replace_overrides') {
    const prepared = prepareOverrides(data.overrides);
    return { slot: 'overrides', apply: () => applyPreparedOverrides(prepared) };
  }
  return { slot: 'overrides', apply: () => (applyClearOverrides(), {}) };
}

function runCommand(apply) {
  try {
    const outcome = apply() || {};
    return { ok: true, skipped: outcome.skipped };
  } catch (error) {
    return failure(error);
  }
}

/* Commands that arrive before WASM is ready are queued and flushed in arrival
   order once it is. Only the latest command per slot survives (the latest
   set_language, and the latest replace_overrides or clear_overrides), since
   each one replaces the whole state of its slot; the survivors keep their
   relative order, so a set_language is still followed by the overrides sent
   after it. Superseded commands are acknowledged with the result of the
   command that superseded them. The number of waiting requests is bounded. */
const MAX_QUEUED_REQUESTS = 64;
const pendingCommands = [];
let pendingRequestCount = 0;

function enqueueCommand(source, requestId, requestType, command) {
  if (pendingRequestCount >= MAX_QUEUED_REQUESTS) {
    throw new TranslationError('queue_full', 'Too many translation commands queued before SQLLY was ready');
  }
  const waiter = { source, requestId, requestType };
  const existing = pendingCommands.findIndex((entry) => entry.slot === command.slot);
  let waiters = [waiter];
  if (existing !== -1) {
    waiters = pendingCommands[existing].waiters.concat(waiters);
    pendingCommands.splice(existing, 1);
  }
  pendingCommands.push({ slot: command.slot, apply: command.apply, waiters });
  pendingRequestCount += 1;
}

function settleQueuedCommands(failure) {
  const queued = pendingCommands.splice(0);
  pendingRequestCount = 0;
  for (const entry of queued) {
    const result = failure ? { ok: false, error: failure } : runCommand(entry.apply);
    for (const waiter of entry.waiters) {
      acknowledge(waiter.source, waiter.requestId, waiter.requestType, result);
    }
  }
}

function onTranslationMessage(event) {
  if (event.origin !== location.origin || event.source !== window.parent) {
    return;
  }
  const data = event.data;
  if (!isPlainObject(data) || data.channel !== TRANSLATION_CHANNEL) {
    return;
  }
  if (typeof data.requestId !== 'string' || !data.requestId) {
    return;
  }
  const requestType = typeof data.type === 'string' ? data.type : 'unknown';
  let command;
  try {
    command = prepareCommand(data);
    if (!wasmModule || !wasmReady) {
      enqueueCommand(event.source, data.requestId, requestType, command);
      return;
    }
  } catch (error) {
    acknowledge(event.source, data.requestId, requestType, failure(error));
    return;
  }
  acknowledge(event.source, data.requestId, requestType, runCommand(command.apply));
}

window.addEventListener('message', onTranslationMessage);

function notifyParentReady() {
  if (window.parent !== window) {
    window.parent.postMessage({ channel: TRANSLATION_CHANNEL, type: 'ready' }, location.origin);
  }
}

/* Start the application and only then open the translation bridge. The
   promise `run` returns resolves after the app's launch callback has run
   `init_ui` — which resets the language and clears overrides — so commands
   applied any earlier would be silently wiped. It rejects when the app never
   launches (browser graphics unavailable); queued commands are then rejected
   rather than acknowledged against an app that does not exist. */
async function launchApplication(wasm, northwindDb, proxy) {
  try {
    await wasm.run(northwindDb, proxy ? proxy.url : undefined, proxy ? proxy.token : undefined);
  } catch (error) {
    settleQueuedCommands(errorText(error));
    throw error;
  }
  wasmModule = wasm;
  wasmReady = true;
  settleQueuedCommands(null);
  notifyParentReady();
}

// Everything else that fits in the second a native SQLLY launch takes.
const FACTS = [
  {
    key: 'browser.boot.fact_light',
    fallback: 'In that same second, light travels most of the way to the Moon.',
  },
  {
    key: 'browser.boot.fact_hummingbird',
    fallback: 'In that same second, a hummingbird beats its wings about 50 times.',
  },
  {
    key: 'browser.boot.fact_cpu',
    fallback: 'In that same second, your CPU runs several billion instructions per core.',
  },
  {
    key: 'browser.boot.fact_space_station',
    fallback: 'In that same second, the International Space Station covers almost 8 km.',
  },
  {
    key: 'browser.boot.fact_formula_one',
    fallback: 'In that same second, a Formula 1 car at top speed travels ~100 meters.',
  },
  {
    key: 'browser.boot.fact_honeybee',
    fallback: 'In that same second, a honeybee flaps its wings around 230 times.',
  },
  {
    key: 'browser.boot.fact_ssd',
    fallback: 'In that same second, your SSD can read a few gigabytes.',
  },
  {
    key: 'browser.boot.fact_sound',
    fallback: 'In that same second, sound travels about 343 meters through the room.',
  },
  {
    key: 'browser.boot.fact_tokenizer',
    fallback: "In that same second, SQLLY's tokenizer chews through millions of lines of SQL.",
  },
  {
    key: 'browser.boot.fact_earth',
    fallback: 'In that same second, Earth carries you ~30 km along its orbit.',
  },
];
// Served by a relay proxy or a SQLLY Embedded bridge: real servers, so none
// of the demo copy applies (no "in-tab demo", no desktop pitch, no facts
// about the second a native launch takes).
const SERVED = proxyConfig() !== null;
let factIndex = Math.floor(Math.random() * FACTS.length);

function refreshBootText() {
  const ready = statusState.key === 'browser.boot.ready';
  const titleKey = SERVED
    ? ready
      ? 'browser.boot.served_ready_title'
      : 'browser.boot.served_title'
    : ready
      ? 'browser.boot.preview_ready_title'
      : 'browser.boot.preview_title';
  const titleFallback = SERVED
    ? ready
      ? 'SQLLY — ready'
      : 'SQLLY'
    : ready
      ? 'SQLLY browser preview (in-tab demo) — ready'
      : 'SQLLY browser preview (in-tab demo)';
  document.title = message(titleKey, titleFallback);
  statusEl.textContent = message(statusState.key, statusState.fallback, statusState.arguments);
  const lead = document.querySelector('.native-note .lead');
  if (lead) {
    lead.textContent = SERVED
      ? message(
          'browser.boot.served_lead',
          'SQLLY is connecting to the databases your administrator set up. Passwords stay on the server.',
        )
      : message(
          'browser.boot.preview_lead',
          'SQLLY browser preview (in-tab demo). It cannot reach your servers. Installed on your machine, SQLLY is ready in under a second.',
        );
  }
  const goEl = document.getElementById('go');
  if (goEl) {
    goEl.textContent = message('browser.boot.go_button', 'Go');
  }
  if (document.body.contains(factEl)) {
    const fact = FACTS[factIndex];
    factEl.textContent = message(fact.key, fact.fallback);
  }
}

if (SERVED) {
  factEl.remove();
}
refreshBootText();
const factTimer = setInterval(() => {
  if (SERVED || !document.body.contains(factEl)) {
    clearInterval(factTimer);
    return;
  }
  factEl.classList.add('fading');
  setTimeout(() => {
    factIndex = (factIndex + 1) % FACTS.length;
    const fact = FACTS[factIndex];
    factEl.textContent = message(fact.key, fact.fallback);
    factEl.classList.remove('fading');
  }, 400);
}, 3200);

const mb = (bytes) => (bytes / 1048576).toFixed(1);

/* Relay browser-proxy mode. A `?proxy=wss://relay.example:8444/proxy` query
   parameter (with an optional `&token=…`), or the `SQLLY_PROXY_URL` /
   `SQLLY_PROXY_TOKEN` globals an embedding page may set, points the app at a
   sqlly-relay started with `--proxy-credentials`. The app then serves that
   relay's connections (read-only) instead of the in-tab samples, so the
   sample downloads are skipped entirely. */
function proxyConfig() {
  const params = new URLSearchParams(location.search);
  const url = params.get('proxy') || globalThis.SQLLY_PROXY_URL || '';
  const token = params.get('token') || globalThis.SQLLY_PROXY_TOKEN || '';
  return url.trim() ? { url: url.trim(), token: token.trim() } : null;
}

function setPhase(key, fallback, arguments_ = {}) {
  statusState = { key, fallback, arguments: arguments_ };
  statusEl.textContent = message(key, fallback, arguments_);
  progressEl.classList.add('indeterminate');
}

function setProgress(received, total) {
  if (total > 0) {
    progressEl.classList.remove('indeterminate');
    progressEl.style.width = `${Math.min(100, (received / total) * 100)}%`;
    statusState = {
      key: 'browser.boot.downloading_progress',
      fallback: 'Downloading {received_mb} / {total_mb} MB',
      arguments: { received_mb: mb(received), total_mb: mb(total) },
    };
    statusEl.textContent = message(
      statusState.key,
      statusState.fallback,
      statusState.arguments,
    );
  } else {
    // No Content-Length (e.g. a compressing proxy): count bytes only.
    setPhase('browser.boot.downloading', 'Downloading {received_mb} MB…', {
      received_mb: mb(received),
    });
  }
}

/* Fetch the wasm while counting bytes through a TransformStream; the
   re-wrapped Response keeps headers so streaming compilation still
   applies where the server sends application/wasm. */
async function fetchWasmWithProgress(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(
      message('browser.boot.download_failed', 'Download failed: HTTP {status}', {
        status: response.status,
      }),
    );
  }
  const total = Number(response.headers.get('Content-Length')) || 0;
  if (!response.body) {
    return response;
  }
  let received = 0;
  const counter = new TransformStream({
    transform(chunk, controller) {
      received += chunk.byteLength;
      setProgress(received, total);
      controller.enqueue(chunk);
    },
  });
  return new Response(response.body.pipeThrough(counter), {
    status: response.status,
    statusText: response.statusText,
    headers: response.headers,
  });
}

/* Boot PGlite (Postgres compiled to wasm, staged into dist/pglite by
   build-browser.sh) and load the Chinook sample into it, then expose
   the bridge the app's Postgres-typed sample connection calls. Failure
   is non-fatal: the Chinook connection reports the engine as
   unavailable, and the SQLite sample still works. */
async function bootPglite() {
  const { PGlite } = await import('./pglite/index.js');
  const response = await fetch('./chinook_postgres.sql');
  if (!response.ok) {
    throw new Error(`chinook download failed: HTTP ${response.status}`);
  }
  const raw = await response.text();
  // The upstream script targets psql: strip the DROP/CREATE DATABASE
  // statements and the \c meta-command — PGlite is a single-database
  // Postgres.
  const sql = raw
    .split('\n')
    .filter(
      (line) =>
        !/^\s*\\/.test(line) && !/^\s*(DROP|CREATE)\s+DATABASE/i.test(line),
    )
    .join('\n');
  const pg = new PGlite();
  await pg.exec(sql);
  // Cells cross the bridge as JSON: PGlite hands back JS values, so
  // re-encode the ones JSON.stringify cannot take as-is. Dates already
  // serialize to ISO strings via toJSON.
  const replacer = (key, value) => {
    if (typeof value === 'bigint') {
      const n = Number(value);
      return Number.isSafeInteger(n) ? n : value.toString();
    }
    if (value instanceof Uint8Array) {
      return '\\x' + Array.from(value, (b) => b.toString(16).padStart(2, '0')).join('');
    }
    return value;
  };
  globalThis.sqllyPglite = {
    async exec(sql) {
      try {
        const results = await pg.exec(sql, { rowMode: 'array' });
        return JSON.stringify(
          {
            ok: true,
            results: results.map((r) => ({
              fields: r.fields.map((f) => ({ name: f.name, dataTypeID: f.dataTypeID })),
              rows: r.rows,
              affectedRows: r.affectedRows ?? 0,
            })),
          },
          replacer,
        );
      } catch (error) {
        return JSON.stringify({
          ok: false,
          error: String((error && error.message) || error),
        });
      }
    },
  };
}

/* Start the spill worker (spill-worker.js) and expose the bridge the app
   uses to keep large results in the Origin Private File System instead of
   in memory. Without OPFS or workers the bridge is left out and results
   stay in memory, as before. See
   crates/sqlly-services/src/browser/spill.rs. */
function installSpillBridge() {
  if (typeof Worker === 'undefined' || typeof navigator.storage?.getDirectory !== 'function') {
    return;
  }
  let worker;
  try {
    // Beside boot.js, like the other files it fetches page-relative.
    worker = new Worker('./spill-worker.js');
  } catch (error) {
    console.warn('spill worker unavailable; large results stay in memory', error);
    return;
  }
  let nextId = 1;
  const pending = new Map();
  worker.onmessage = ({ data }) => {
    const waiter = pending.get(data.id);
    if (!waiter) return;
    pending.delete(data.id);
    if (data.ok) waiter.resolve(data.value);
    else waiter.reject(new Error(data.error));
  };
  worker.onerror = (event) => {
    console.warn('spill worker failed', event.message);
    for (const waiter of pending.values()) waiter.reject(new Error('the spill worker stopped'));
    pending.clear();
  };
  const call = (op, args = {}, transfer = []) =>
    new Promise((resolve, reject) => {
      const id = nextId++;
      pending.set(id, { resolve, reject });
      worker.postMessage({ id, op, args }, transfer);
    });
  // No reply: writes and removals run in order with everything else, and a
  // failed write shows up on the next read of that spill.
  const send = (op, args, transfer = []) => worker.postMessage({ id: 0, op, args }, transfer);
  globalThis.sqllySpill = {
    supported: () => call('supported'),
    create: (name) => send('create', { name }),
    append: (name, data, index) =>
      send('append', { name, data: data.buffer, index: index.buffer }, [data.buffer, index.buffer]),
    page: (name, start, end) => call('page', { name, start, end }).then((b) => new Uint8Array(b)),
    body: (name) => call('body', { name }).then((b) => new Uint8Array(b)),
    remove: (name) => send('remove', { name }),
    sweep: () => call('sweep'),
    estimate: () => call('estimate'),
  };
}

async function boot() {
  try {
    installSpillBridge();
    const proxy = proxyConfig();
    // Postgres boots and Chinook loads while the app wasm downloads —
    // unless a relay proxy provides the connections, in which case no sample
    // is needed.
    const pgliteBoot = proxy
      ? Promise.resolve()
      : bootPglite().catch((error) => {
          console.warn(
            'PGlite/Chinook unavailable; the Chinook sample connection will not work',
            error,
          );
        });
    setPhase('browser.boot.loading_application', 'Loading application…');
    const wasm = await import('./sqlly_browser.js');
    const wasmResponse = fetchWasmWithProgress('./sqlly_browser_bg.wasm');
    await wasm.default({ module_or_path: wasmResponse });

    // Fetch the bundled Northwind sample database (staged into dist/ by
    // build-browser.sh, served same-origin), reusing the byte-counting
    // progress since it is larger than the wasm itself. Any failure is
    // non-fatal: an empty array tells the app to fall back to the
    // emulated demo database.
    let northwindDb = new Uint8Array(0);
    if (!proxy) {
      try {
        const dbResponse = await fetchWasmWithProgress('./northwind.db');
        northwindDb = new Uint8Array(await dbResponse.arrayBuffer());
      } catch (dbError) {
        console.warn('northwind.db unavailable; using the demo database', dbError);
      }
    }

    setPhase(
      proxy ? 'browser.boot.connecting' : 'browser.boot.starting',
      proxy ? 'Connecting to your server…' : 'Starting SQLLY…',
    );
    await launchApplication(wasm, northwindDb, proxy);

    // Hold the reveal until Chinook is queryable (or its load failed),
    // so connecting to the PGlite sample immediately after "Go" never
    // races the data load.
    if (!proxy) {
      setPhase('browser.boot.loading_chinook', 'Loading the Chinook sample database…');
    }
    await pgliteBoot;
    // The app is live behind the overlay; hold the reveal for a
    // deliberate click so the workbench never pops in mid-paint.
    setPhase('browser.boot.ready', 'Ready.');
    progressEl.classList.remove('indeterminate');
    progressEl.style.width = '100%';
    const goEl = document.getElementById('go');
    goEl.style.display = 'inline-block';
    goEl.focus();
    refreshBootText();
    goEl.addEventListener('click', () => {
      clearInterval(factTimer);
      loadingEl.remove();
    });
  } catch (error) {
    console.error('Failed to initialize SQLLY:', error);
    if (!wasmReady) {
      settleQueuedCommands(errorText(error));
    }
    if (loadingEl) {
      loadingEl.replaceChildren();
      const box = document.createElement('div');
      box.className = 'error';
      const heading = document.createElement('h2');
      heading.textContent = message('browser.boot.load_failed_title', 'Failed to load SQLLY');
      const messageEl = document.createElement('p');
      messageEl.textContent = errorText(error);
      const hint = document.createElement('p');
      hint.style.marginTop = '10px';
      hint.textContent = message(
        'browser.boot.console_hint',
        'See the browser console for details.',
      );
      box.append(heading, messageEl, hint);
      loadingEl.append(box);
    }
  }
}
boot();
