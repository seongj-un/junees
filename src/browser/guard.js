// The input guard of the `browser` tool, and the risk rules it shares
// with the snapshot. Runs in the isolated world "junees" of every frame
// of the page at document start (Page.addScriptToEvaluateOnNewDocument),
// before any script of the page, and again in front of every snapshot
// (it does nothing when it is already there).
//
// The tool checks an element (is it a password field, does it submit a
// password form, ...) in one call and moves the mouse or presses the key
// in the next. A page can change what is under the pointer or where the
// focus is in between (a timer, a mousemove handler). The guard closes
// that gap: its capture listeners on the window run first for every
// mouse and key event, and stop a trusted event (only the tool makes
// trusted input events in this browser) whose target needs the user's
// approval, unless the tool armed it for exactly that element (approved)
// or that risk (approved in a frame; see `arm`).
(() => {
  if (globalThis.__juneesGuard) return;
  const LABEL = 80;
  const ROLES = new Set(['button', 'link', 'checkbox', 'radio', 'tab', 'menuitem', 'menuitemcheckbox',
    'menuitemradio', 'option', 'switch', 'textbox', 'combobox', 'searchbox', 'slider', 'spinbutton', 'treeitem']);
  const PAYMENT_NAME = /(card.?num|cc.?(num|no)|cvc|cvv|csc|security.?code|expir|exp.?(date|month|year)|iban|account.?num|routing)/i;
  const PAYMENT_LABEL = /\b(pay|payment|purchase|buy|checkout|check out|place order|order now|subscribe|donate|transfer|send money)\b|결제|구매|주문|송금|이체|후원/i;
  // The risk of a key press or click that goes into a frame whose page
  // cannot be read (another site): what it does there is unknown.
  const FRAME_RISK = 'goes into a frame from another site, whose fields and buttons cannot be checked';
  // The risk of a click or key press that goes into a closed shadow root
  // (its inside is hidden from every script here, this one included).
  const CLOSED_RISK = 'goes into a closed shadow root, whose fields and buttons cannot be checked';
  // Elements the tool found (through DevTools, which sees them) to host a
  // closed shadow root; see `closed` below.
  const closedHosts = new WeakSet();
  // The page has more closed shadow roots than the tool marks one by one:
  // every element counts as in one (see `closedEverywhere`).
  let closedAll = false;

  const clean = (text) => (text || '').replace(/\s+/g, ' ').trim();
  const short = (text, max) => {
    const t = clean(text);
    return t.length > max ? t.slice(0, max) + '...' : t;
  };

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

  // Whether `el` hosts a closed shadow root or sits in the light DOM of
  // one (a slotted element is shown, and clicked, through the hidden
  // tree).
  function inClosed(el) {
    if (closedAll) return true;
    for (let n = el; n; n = n.parentNode && n.parentNode.nodeType === Node.DOCUMENT_FRAGMENT_NODE
      ? n.parentNode.host : n.parentNode) {
      if (closedHosts.has(n)) return true;
    }
    return false;
  }

  // Why typing into `el` needs the user's approval, or ''.
  function typeRisk(el) {
    if (inClosed(el)) return 'a field with a closed shadow root, whose inside cannot be checked';
    if (el.tagName === 'INPUT' && (el.type || '').toLowerCase() === 'password') return 'a password field';
    if (payment(el)) return 'a payment card or bank account field';
    return '';
  }

  // What submitting the form `el` belongs to would send, or ''.
  function formRisk(el) {
    const form = el.form || (el.closest && el.closest('form'));
    if (!form) return '';
    // `form.elements` also holds fields outside the form tied to it by a
    // `form=` attribute.
    const fields = Array.from(form.elements).filter((f) => f.tagName === 'INPUT' || f.tagName === 'SELECT');
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

  // A frame element whose page cannot be read from here (another site).
  function closedFrame(el) {
    if (!el || (el.tagName !== 'IFRAME' && el.tagName !== 'FRAME')) return false;
    let inner = null;
    try { inner = el.contentDocument; } catch (e) { inner = null; }
    return !inner;
  }

  // Why clicking `el` needs the user's approval, or ''.
  function clickRisk(el) {
    if (closedFrame(el)) return FRAME_RISK;
    if (inClosed(el)) return CLOSED_RISK;
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
    if (closedFrame(el)) return FRAME_RISK;
    if (inClosed(el)) return CLOSED_RISK;
    const k = kind(el);
    if (k === 'textbox' || k === 'password' || k === 'checkbox' || k === 'radio' || k === 'select') return formRisk(el);
    return clickRisk(el);
  }

  // Why pressing Space in `el` needs the user's approval, or ''. Space
  // presses a button or toggles a checkbox like a click, and sends no
  // form from a field (only Enter does).
  function spaceRisk(el) {
    if (!el) return '';
    return clickRisk(el);
  }

  // The risk of clicking at `el` (what a click there activates).
  function pointRisk(el) {
    if (inClosed(el)) return CLOSED_RISK;
    return clickRisk((el.closest && el.closest('a, button, input, [role]')) || el);
  }

  const MOUSE = new Set(['pointerdown', 'pointerup', 'mousedown', 'mouseup', 'click', 'dblclick', 'auxclick']);
  const KEYS = new Set(['keydown', 'keypress', 'keyup']);
  // What the tool armed: { el, approved, key, risk, any, until }.
  let armed = null;
  const stopped = [];

  function risk(event, target) {
    if (MOUSE.has(event.type)) return pointRisk(target);
    if (event.key === 'Enter') return enterRisk(target);
    if (event.key === ' ') return spaceRisk(target);
    return '';
  }

  function allowed(event, target) {
    const arm = armed && performance.now() < armed.until ? armed : null;
    if (arm && arm.approved && arm.el) {
      if (target === arm.el) return '';
      // Enter in a field sends its form as a click on the form's submit
      // button.
      if (arm.key === 'Enter' && event.type === 'click' && arm.el.form && target.form === arm.el.form
          && submits(target)) return '';
      if (!arm.key && MOUSE.has(event.type)) {
        if (arm.el.contains(target)) return '';
        if (arm.el.labels && Array.from(arm.el.labels).some((l) => l.contains(target))) return '';
      }
    }
    if (arm && arm.any) return '';
    const now = risk(event, target);
    if (!now || (arm && arm.risk && now === arm.risk)) return '';
    return now;
  }

  function guard(event) {
    if (!event.isTrusted) return;
    const target = event.composedPath()[0];
    if (!target || target.nodeType !== Node.ELEMENT_NODE) return;
    let why;
    try { why = allowed(event, target); } catch (e) { why = 'could not be checked'; }
    if (!why) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    if (stopped.length < 10) {
      const name = target.tagName.toLowerCase() + (label(target) ? ' "' + label(target) + '"' : '');
      stopped.push({ event: event.type, target: name, risk: why });
    }
  }

  for (const type of [...MOUSE, ...KEYS]) addEventListener(type, guard, true);

  globalThis.__juneesLib = {
    clean, short, kind, label, typeRisk, formRisk, clickRisk, enterRisk, spaceRisk, pointRisk, closedFrame,
    inClosed, closedHosts, FRAME_RISK, CLOSED_RISK,
  };
  globalThis.__juneesGuard = {
    // Arms the guard for the next input events in this frame, for 5 s:
    // `el` (approved) is the element the user approved, which then takes
    // the events whatever its risk (`key`: the key pressed; Enter may
    // also click its form's submit button); `risk` lets events through whose
    // target has exactly that risk (an approved element in a frame,
    // which the top frame's call cannot point at here); `any` lets every
    // event through (an approved key press into a frame of another
    // site). `null` disarms it.
    arm(arm) {
      armed = arm ? Object.assign({ until: performance.now() + 5000 }, arm) : null;
      stopped.length = 0;
    },
    // Marks `el` as the host of a closed shadow root (the tool calls it
    // on each host DevTools finds, before every check): the risk rules
    // then treat a click or key press into it like one into a frame of
    // another site.
    closed(el) {
      closedHosts.add(el);
    },
    // The page has too many closed shadow roots to mark each host (the
    // tool caps the work per check): every click, Enter/Space and typing
    // then needs the user's approval, like one into a closed root. Stays
    // for this document.
    closedEverywhere() {
      closedAll = true;
    },
    // The events stopped since the guard was armed, and disarms it.
    take() {
      armed = null;
      return stopped.splice(0);
    },
  };
})();
