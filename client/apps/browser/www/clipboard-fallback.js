// Copy and Cut for pages served over plain HTTP.
//
// gpui's web platform writes the clipboard with
// `navigator.clipboard.writeText` and nothing else. Browsers only expose
// `navigator.clipboard` in a secure context (HTTPS or localhost), so a page
// served over plain HTTP — a SQLLY Embedded install on a LAN address without
// a TLS certificate, say — had no working Copy or Cut, and the call threw
// inside the app. This script runs before the app loads and:
//
// - when `navigator.clipboard` is missing, supplies one whose `writeText`
//   copies through a hidden textarea and `document.execCommand('copy')`;
// - when it exists but refuses a write, retries the same way.
//
// Paste needs nothing here: it comes from the browser's own `paste` event,
// which works on any page. The stand-in's `read` and `readText` reject with
// NotAllowedError, so nothing calls a missing method.
//
// A classic script (not a module) so it runs, in order, before boot.js.
(function installClipboardFallback(global) {
  const doc = global.document;
  const nav = global.navigator;
  if (!doc || !nav) {
    return;
  }

  function copyWithExecCommand(text) {
    const parent = doc.body || doc.documentElement;
    if (!parent || typeof doc.execCommand !== 'function') {
      return false;
    }
    const previous = doc.activeElement;
    const area = doc.createElement('textarea');
    area.value = text;
    area.setAttribute('readonly', '');
    area.setAttribute('aria-hidden', 'true');
    area.setAttribute('tabindex', '-1');
    Object.assign(area.style, {
      position: 'fixed',
      top: '0',
      left: '0',
      width: '1px',
      height: '1px',
      opacity: '0',
      pointerEvents: 'none',
    });
    parent.appendChild(area);
    let copied = false;
    try {
      area.focus();
      area.select();
      copied = doc.execCommand('copy') === true;
    } catch {
      copied = false;
    } finally {
      area.remove();
      // Hand focus back to the app's hidden input so typing keeps working.
      if (previous && previous !== area && typeof previous.focus === 'function') {
        try {
          previous.focus({ preventScroll: true });
        } catch {
          // A detached element cannot take focus; nothing to restore.
        }
      }
    }
    return copied;
  }

  function notAllowed(message) {
    const error = new Error(message);
    error.name = 'NotAllowedError';
    return error;
  }

  // Synchronous on purpose: execCommand('copy') only works inside the user
  // gesture (the key press or menu click) that asked for the copy.
  function fallbackWrite(text) {
    return copyWithExecCommand(String(text))
      ? Promise.resolve()
      : Promise.reject(notAllowed('The browser did not allow copying to the clipboard.'));
  }

  const clipboard = nav.clipboard;
  if (!clipboard || typeof clipboard.writeText !== 'function') {
    const unavailable = () =>
      Promise.reject(
        notAllowed('Reading the clipboard needs a secure (HTTPS) page; paste with the keyboard.'),
      );
    const standIn = {
      writeText: fallbackWrite,
      read: unavailable,
      readText: unavailable,
    };
    try {
      Object.defineProperty(nav, 'clipboard', { value: standIn, configurable: true });
    } catch {
      // A locked-down navigator: leave it, nothing else to try.
    }
    return;
  }

  const nativeWrite = clipboard.writeText;
  function writeText(text) {
    const value = String(text);
    let pending;
    try {
      pending = Promise.resolve(nativeWrite.call(clipboard, value));
    } catch (error) {
      pending = Promise.reject(error);
    }
    return pending.catch((error) =>
      copyWithExecCommand(value) ? undefined : Promise.reject(error),
    );
  }
  try {
    Object.defineProperty(clipboard, 'writeText', { value: writeText, configurable: true });
  } catch {
    // Keep the browser's own writeText.
  }
})(globalThis);
