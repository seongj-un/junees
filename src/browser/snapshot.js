// The page snapshot of the `browser` tool. Runs in an isolated world
// (Page.createIsolatedWorld): it shares the page's DOM but not its
// JavaScript, so the page cannot change what these functions see or do
// by replacing built-in methods. Evaluates to the snapshot; the element
// list stays in this world for the actions below. The tool runs guard.js
// in front of it (the world is the one the guard runs in at document
// start).
(() => {
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD', 'META', 'LINK', 'svg', 'SVG']);
  const BLOCK = new Set(['ADDRESS', 'ARTICLE', 'ASIDE', 'BLOCKQUOTE', 'BR', 'DD', 'DETAILS', 'DIALOG',
    'DIV', 'DL', 'DT', 'FIELDSET', 'FIGCAPTION', 'FIGURE', 'FOOTER', 'FORM', 'H1', 'H2', 'H3', 'H4',
    'H5', 'H6', 'HEADER', 'HR', 'LI', 'MAIN', 'NAV', 'OL', 'P', 'PRE', 'SECTION', 'TABLE', 'TD', 'TH',
    'TR', 'UL', 'LEGEND', 'CAPTION']);
  // The risk rules, shared with the input guard (guard.js, which runs
  // in front of this script).
  const {
    clean, short, kind, label, typeRisk, clickRisk, enterRisk, pointRisk, closedFrame, FRAME_RISK,
  } = globalThis.__juneesLib;
  const LABEL = 80;

  function visible(el) {
    if (el.hidden) return false;
    const style = el.ownerDocument.defaultView.getComputedStyle(el);
    if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse') return false;
    if (style.display === 'contents') return true;
    return el.getClientRects().length > 0;
  }

  function describe(el) {
    const parts = [];
    const k = kind(el);
    if (k === 'link') {
      const href = el.getAttribute('href') || '';
      if (href && !href.startsWith('javascript:')) parts.push('-> ' + short(href, 120));
    }
    if (k === 'textbox' && el.tagName !== 'DIV') {
      const value = el.value;
      if (value) parts.push('value="' + short(value, LABEL) + '"');
    }
    if (k === 'password' && el.value) parts.push('(filled)');
    if ((k === 'checkbox' || k === 'radio') && el.checked) parts.push('checked');
    if (k === 'select') {
      const chosen = el.selectedOptions && el.selectedOptions[0];
      if (chosen) parts.push('selected="' + short(chosen.text, LABEL) + '"');
      const options = Array.from(el.options).slice(0, 20).map((o) => short(o.text, 40));
      parts.push('options: ' + options.join(' | ') + (el.options.length > 20 ? ' | ...' : ''));
    }
    if (el.disabled) parts.push('disabled');
    return parts.length ? ' ' + parts.join(' ') : '';
  }

  const refs = [];
  const info = [];
  const lines = [];
  let line = '';
  const flush = () => {
    const text = clean(line);
    if (text) lines.push(text);
    line = '';
  };

  function walk(node) {
    if (node.nodeType === Node.TEXT_NODE) {
      line += node.nodeValue;
      return;
    }
    if (node.nodeType !== Node.ELEMENT_NODE && node.nodeType !== Node.DOCUMENT_FRAGMENT_NODE) return;
    if (node.nodeType === Node.ELEMENT_NODE) {
      const el = node;
      if (SKIP.has(el.tagName) || !visible(el)) return;
      if (el.tagName === 'IFRAME' || el.tagName === 'FRAME') {
        flush();
        let inner = null;
        try { inner = el.contentDocument; } catch (e) { inner = null; }
        if (inner && inner.body) {
          walk(inner.body);
          flush();
        } else {
          lines.push('[frame from another site: not shown]');
        }
        return;
      }
      const k = kind(el);
      const block = BLOCK.has(el.tagName);
      if (block) flush();
      if (/^H[1-6]$/.test(el.tagName)) line += '#'.repeat(Number(el.tagName[1])) + ' ';
      if (k) {
        refs.push(el);
        const n = refs.length;
        info.push({ kind: k, type_risk: typeRisk(el), click_risk: clickRisk(el), enter_risk: enterRisk(el) });
        line += ' [' + n + ' ' + k + ' "' + label(el) + '"' + describe(el) + '] ';
        // The label already holds the text of buttons, links and fields.
        if (['link', 'button', 'textbox', 'password', 'select', 'option', 'tab', 'menuitem'].includes(k)
            || el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {
          if (block) flush();
          return;
        }
      } else if (el.tagName === 'IMG' && el.getAttribute('alt')) {
        line += ' [image "' + short(el.getAttribute('alt'), LABEL) + '"] ';
      }
      if (el.shadowRoot) walk(el.shadowRoot);
    }
    for (const child of node.childNodes) walk(child);
    if (node.nodeType === Node.ELEMENT_NODE && BLOCK.has(node.tagName)) flush();
  }

  if (document.body) walk(document.body);
  flush();

  // The element `n` (1-based) of this snapshot, if it is still there.
  function element(n) {
    const el = refs[n - 1];
    if (!el || !el.isConnected) return null;
    return el;
  }

  // Whether a click at `hit` lands on `el` (or a label of it).
  function onElement(el, hit) {
    if (!hit || hit === el || el.contains(hit) || hit.contains(el)) return true;
    return !!(el.labels && Array.from(el.labels).some((l) => l.contains(hit)));
  }

  function name(el) {
    return el.tagName.toLowerCase() + (label(el) ? ' "' + label(el) + '"' : '');
  }

  // Where to click element `n`: the middle of it after scrolling it into
  // view, in the top page's coordinates; what covers it; and its risk now.
  // Arms the input guard in the same call: from here on a mouse event
  // whose target needs approval goes through only when it is `el` and
  // `approved` (the question named this element).
  function point(n, approved) {
    const el = element(n);
    if (!el) return { error: 'gone' };
    el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    const rect = el.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return { error: 'not visible' };
    let x = rect.left + rect.width / 2;
    let y = rect.top + rect.height / 2;
    const hit = el.ownerDocument.elementFromPoint(x, y);
    let win = el.ownerDocument.defaultView;
    while (win.frameElement) {
      const frame = win.frameElement.getBoundingClientRect();
      x += frame.left + win.frameElement.clientLeft;
      y += frame.top + win.frameElement.clientTop;
      win = win.frameElement.ownerDocument.defaultView;
    }
    let covered = '';
    if (!onElement(el, hit)) {
      covered = name(hit);
      const risk = pointRisk(hit);
      if (risk) return { error: 'covered', covered, risk };
    }
    globalThis.__juneesGuard.arm({ el, approved: !!approved });
    return {
      x, y, covered, click_risk: clickRisk(el), kind: kind(el), in_frame: el.ownerDocument !== document,
    };
  }

  // The element at (`x`, `y`) of the top page, inside frames of this site
  // and open shadow roots too.
  function hitAt(x, y) {
    let doc = document;
    let hit = doc.elementFromPoint(x, y);
    for (;;) {
      if (hit && hit.shadowRoot) {
        const inner = hit.shadowRoot.elementFromPoint(x, y);
        if (inner && inner !== hit) { hit = inner; continue; }
      }
      if (hit && (hit.tagName === 'IFRAME' || hit.tagName === 'FRAME') && !closedFrame(hit)) {
        const frame = hit.getBoundingClientRect();
        x -= frame.left + hit.clientLeft;
        y -= frame.top + hit.clientTop;
        doc = hit.contentDocument;
        const inner = doc.elementFromPoint(x, y);
        if (!inner) return hit;
        hit = inner;
        continue;
      }
      return hit;
    }
  }

  // What is under the pointer at (`x`, `y`) now that it moved there for
  // element `n` (a mousemove handler may have put something else there).
  function recheck(n, x, y) {
    const el = element(n);
    if (!el) return { error: 'gone' };
    const hit = hitAt(x, y);
    if (onElement(el, hit)) return {};
    const risk = pointRisk(hit);
    if (risk) return { error: 'covered', covered: name(hit), risk };
    return {};
  }

  // Types `text` into element `n` (replacing what it holds when `clear`).
  // Focus, checks and typing run in this one call, so no page script runs
  // between the check of where the focus is and the typing (a page that
  // moves the focus to a password field from a focus handler or a timer
  // is caught). Typing into a field with a type risk happens only when
  // `approved` (the question named such a field).
  function typeInto(n, text, clear, approved) {
    const el = element(n);
    if (!el) return { error: 'gone' };
    const k = kind(el);
    if (k !== 'textbox' && k !== 'password') return { error: 'not editable', kind: k };
    if (el.disabled || el.readOnly) return { error: 'read only' };
    el.scrollIntoView({ block: 'center', behavior: 'instant' });
    el.focus();
    const active = deepActive();
    if (active !== el) {
      return { error: 'focus moved', to: active ? kind(active) || active.tagName.toLowerCase() : 'nothing' };
    }
    const risk = typeRisk(el);
    if (risk && !approved) return { error: 'risky', type_risk: risk };
    if (clear) {
      if (typeof el.select === 'function') el.select();
      else el.ownerDocument.getSelection().selectAllChildren(el);
    }
    const doc = el.ownerDocument;
    let typed = true;
    // Clearing an empty field deletes nothing, which is fine.
    if (text === '') { if (clear) doc.execCommand('delete'); }
    else typed = doc.execCommand('insertText', false, text);
    if (deepActive() !== el) return { error: 'focus moved while typing' };
    if (!typed) return { error: 'not typed' };
    return {};
  }

  // Focuses element `n` (when given) for a key press, then reads the
  // risk of Enter or Space (`submits`) in the focused element and arms
  // the input guard, all in this one call: a key event whose target
  // needs approval then goes through only when it is that element and
  // `approved`. `in_frame` says the focus is in a frame (approved key
  // presses there need the guard of that frame armed too).
  function armKey(n, submits, approved) {
    if (n) {
      const el = element(n);
      if (!el) return { error: 'gone' };
      el.scrollIntoView({ block: 'center', behavior: 'instant' });
      el.focus();
    }
    const active = deepActive();
    globalThis.__juneesGuard.arm({ el: active, approved: !!approved, key: true });
    return {
      risk: submits ? enterRisk(active) : '',
      in_frame: !!active && (active.ownerDocument !== document || closedFrame(active)),
    };
  }

  // Chooses the option of select `n` whose text or value is `wanted`.
  function choose(n, wanted) {
    const el = element(n);
    if (!el) return { error: 'gone' };
    const want = clean(wanted).toLowerCase();
    const options = Array.from(el.options);
    const option = options.find((o) => clean(o.text).toLowerCase() === want || o.value.toLowerCase() === want)
      || options.find((o) => clean(o.text).toLowerCase().includes(want));
    if (!option) return { error: 'no option', options: options.slice(0, 30).map((o) => clean(o.text)) };
    el.value = option.value;
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return { chosen: clean(option.text) };
  }

  function deepActive() {
    let el = document.activeElement;
    for (;;) {
      if (el && el.shadowRoot && el.shadowRoot.activeElement) { el = el.shadowRoot.activeElement; continue; }
      if (el && (el.tagName === 'IFRAME' || el.tagName === 'FRAME')) {
        let inner = null;
        try { inner = el.contentDocument; } catch (e) { inner = null; }
        if (inner && inner.activeElement) { el = inner.activeElement; continue; }
      }
      return el;
    }
  }

  // The value of element `n` after typing (passwords are not shown).
  function value(n) {
    const el = element(n);
    if (!el) return { error: 'gone' };
    if (kind(el) === 'password') return { value: el.value ? '(filled)' : '' };
    return { value: short(el.value !== undefined ? el.value : el.innerText, 200) };
  }

  globalThis.__junees = {
    point, recheck, typeInto, armKey, choose, value,
    // The input events the guard stopped since it was armed; disarms it.
    stopped: () => globalThis.__juneesGuard.take(),
  };
  return {
    title: document.title,
    url: location.href,
    text: lines.join('\n'),
    refs: info,
    // The focus is in a frame of another site (Enter there is unchecked).
    frame_focus: closedFrame(deepActive()),
  };
})()
