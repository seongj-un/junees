// The page snapshot of the `browser` tool. Runs in an isolated world
// (Page.createIsolatedWorld): it shares the page's DOM but not its
// JavaScript, so the page cannot change what these functions see or do
// by replacing built-in methods. Evaluates to the snapshot; the element
// list stays in this world for the actions below.
(() => {
  const LABEL = 80;
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD', 'META', 'LINK', 'svg', 'SVG']);
  const BLOCK = new Set(['ADDRESS', 'ARTICLE', 'ASIDE', 'BLOCKQUOTE', 'BR', 'DD', 'DETAILS', 'DIALOG',
    'DIV', 'DL', 'DT', 'FIELDSET', 'FIGCAPTION', 'FIGURE', 'FOOTER', 'FORM', 'H1', 'H2', 'H3', 'H4',
    'H5', 'H6', 'HEADER', 'HR', 'LI', 'MAIN', 'NAV', 'OL', 'P', 'PRE', 'SECTION', 'TABLE', 'TD', 'TH',
    'TR', 'UL', 'LEGEND', 'CAPTION']);
  const ROLES = new Set(['button', 'link', 'checkbox', 'radio', 'tab', 'menuitem', 'menuitemcheckbox',
    'menuitemradio', 'option', 'switch', 'textbox', 'combobox', 'searchbox', 'slider', 'spinbutton', 'treeitem']);
  const PAYMENT_NAME = /(card.?num|cc.?(num|no)|cvc|cvv|csc|security.?code|expir|exp.?(date|month|year)|iban|account.?num|routing)/i;
  const PAYMENT_LABEL = /\b(pay|payment|purchase|buy|checkout|check out|place order|order now|subscribe|donate|transfer|send money)\b|결제|구매|주문|송금|이체|후원/i;

  const clean = (text) => (text || '').replace(/\s+/g, ' ').trim();
  const short = (text, max) => {
    const t = clean(text);
    return t.length > max ? t.slice(0, max) + '...' : t;
  };

  function visible(el) {
    if (el.hidden) return false;
    const style = el.ownerDocument.defaultView.getComputedStyle(el);
    if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse') return false;
    if (style.display === 'contents') return true;
    return el.getClientRects().length > 0;
  }

  function kind(el) {
    const tag = el.tagName;
    if (tag === 'A' && el.hasAttribute('href')) return 'link';
    if (tag === 'BUTTON') return 'button';
    if (tag === 'SELECT') return 'select';
    if (tag === 'TEXTAREA') return 'textbox';
    if (tag === 'SUMMARY') return 'button';
    if (tag === 'INPUT') {
      const type = (el.type || 'text').toLowerCase();
      if (type === 'hidden') return null;
      if (['submit', 'button', 'reset', 'image'].includes(type)) return 'button';
      if (['checkbox', 'radio', 'file', 'range', 'color'].includes(type)) return type;
      if (type === 'password') return 'password';
      return 'textbox';
    }
    if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return 'textbox';
    const role = (el.getAttribute('role') || '').toLowerCase();
    if (ROLES.has(role)) return role;
    if (el.hasAttribute('onclick')) return 'clickable';
    return null;
  }

  function label(el) {
    const aria = el.getAttribute('aria-label');
    if (aria) return short(aria, LABEL);
    const by = el.getAttribute('aria-labelledby');
    if (by) {
      const text = by.split(/\s+/).map((id) => {
        const target = el.ownerDocument.getElementById(id);
        return target ? target.innerText || target.textContent : '';
      }).join(' ');
      if (clean(text)) return short(text, LABEL);
    }
    if (el.labels && el.labels.length) {
      const text = Array.from(el.labels).map((l) => l.innerText).join(' ');
      if (clean(text)) return short(text, LABEL);
    }
    if (el.tagName === 'INPUT' && ['submit', 'button', 'reset'].includes((el.type || '').toLowerCase())) {
      return short(el.value || el.type, LABEL);
    }
    const text = el.tagName === 'SELECT' ? '' : clean(el.innerText);
    if (text) return short(text, LABEL);
    for (const name of ['placeholder', 'title', 'alt', 'name']) {
      const value = el.getAttribute(name);
      if (value) return short(value, LABEL);
    }
    const img = el.querySelector && el.querySelector('img[alt]');
    if (img) return short(img.getAttribute('alt'), LABEL);
    return '';
  }

  function payment(el) {
    if (el.tagName !== 'INPUT' && el.tagName !== 'SELECT') return false;
    const auto = (el.getAttribute('autocomplete') || '').toLowerCase();
    if (/(^|\s)cc-/.test(auto)) return true;
    return PAYMENT_NAME.test((el.name || '') + ' ' + (el.id || ''));
  }

  // Why typing into `el` needs the user's approval, or ''.
  function typeRisk(el) {
    if (el.tagName === 'INPUT' && (el.type || '').toLowerCase() === 'password') return 'a password field';
    if (payment(el)) return 'a payment card or bank account field';
    return '';
  }

  // What submitting the form `el` belongs to would send, or ''.
  function formRisk(el) {
    const form = el.form || (el.closest && el.closest('form'));
    if (!form) return '';
    const fields = Array.from(form.querySelectorAll('input, select'));
    if (fields.some((f) => f.tagName === 'INPUT' && (f.type || '').toLowerCase() === 'password')) {
      return 'submits a form with a password field';
    }
    if (fields.some(payment)) return 'submits a form with payment fields';
    return '';
  }

  function submits(el) {
    if (el.tagName === 'BUTTON') return (el.getAttribute('type') || 'submit').toLowerCase() === 'submit';
    if (el.tagName === 'INPUT') return ['submit', 'image'].includes((el.type || '').toLowerCase());
    return false;
  }

  // Why clicking `el` needs the user's approval, or ''.
  function clickRisk(el) {
    const k = kind(el);
    if (!k || ['textbox', 'password', 'select', 'checkbox', 'radio', 'range', 'color', 'file'].includes(k)) return '';
    if (submits(el)) {
      const risk = formRisk(el);
      if (risk) return risk;
    }
    const text = label(el);
    if (PAYMENT_LABEL.test(text)) return 'looks like a payment or purchase button ("' + text + '")';
    return '';
  }

  // Why pressing Enter in `el` needs the user's approval, or ''.
  function enterRisk(el) {
    if (!el) return '';
    const k = kind(el);
    if (k === 'textbox' || k === 'password' || k === 'checkbox' || k === 'radio' || k === 'select') return formRisk(el);
    return clickRisk(el);
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

  // Where to click element `n`: the middle of it after scrolling it into
  // view, in the top page's coordinates; what covers it; and its risk now.
  function point(n) {
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
    const inLabel = el.labels && Array.from(el.labels).some((l) => l.contains(hit));
    const fine = !hit || hit === el || el.contains(hit) || hit.contains(el) || inLabel;
    let covered = '';
    if (!fine) {
      covered = hit.tagName.toLowerCase() + (label(hit) ? ' "' + label(hit) + '"' : '');
      const risk = clickRisk(hit.closest('a, button, input, [role]') || hit);
      if (risk) return { error: 'covered', covered, risk };
    }
    return { x, y, covered, click_risk: clickRisk(el), kind: kind(el) };
  }

  // Focuses element `n` for typing, selecting what it holds when `clear`.
  function focus(n, clear) {
    const el = element(n);
    if (!el) return { error: 'gone' };
    const k = kind(el);
    if (k !== 'textbox' && k !== 'password') return { error: 'not editable', kind: k };
    if (el.disabled || el.readOnly) return { error: 'read only' };
    el.scrollIntoView({ block: 'center', behavior: 'instant' });
    el.focus();
    if (clear) {
      if (typeof el.select === 'function') el.select();
      else el.ownerDocument.getSelection().selectAllChildren(el);
    }
    return { type_risk: typeRisk(el), focused: deepActive() === el };
  }

  // Focuses element `n` before a key press.
  function focusOnly(n) {
    const el = element(n);
    if (!el) return { error: 'gone' };
    el.scrollIntoView({ block: 'center', behavior: 'instant' });
    el.focus();
    return {};
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
    point, focus, focusOnly, choose, value,
    enterRisk: () => enterRisk(deepActive()),
  };
  return {
    title: document.title,
    url: location.href,
    text: lines.join('\n'),
    refs: info,
  };
})()
