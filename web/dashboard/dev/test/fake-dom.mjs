// A small stand-in for the browser DOM, enough for the screens' render tests
// under `node --test`. It is not a test file itself (no `.test.mjs` name).
//
// What it models on purpose, because the screens rely on it:
// - attributes and the `hidden` / `disabled` / `id` properties that mirror them
// - moving a node detaches it from its old parent, and detaching the focused
//   node (or an ancestor) drops focus to the body, as browsers do
// - `detached` counts how often a node left its parent, so a test can tell a
//   node that stayed mounted from one that was put back

class FakeNode {
  parentNode = null;
  childNodes = [];
  detached = 0;

  get isConnected() {
    for (let node = this; node; node = node.parentNode) if (node === document.body) return true;
    return false;
  }

  contains(other) {
    for (let node = other; node; node = node.parentNode) if (node === this) return true;
    return false;
  }

  remove() {
    const parent = this.parentNode;
    if (!parent) return;
    if (this.contains(document.activeElement)) document.activeElement = document.body;
    parent.childNodes.splice(parent.childNodes.indexOf(this), 1);
    this.parentNode = null;
    this.detached += 1;
  }
}

class FakeText extends FakeNode {
  constructor(data) {
    super();
    this.data = String(data);
  }

  get textContent() {
    return this.data;
  }
}

/** `tag`, `#id`, `.class`, `[attr]` and `[attr="value"]`, combined in one compound. */
function compound(selector) {
  const tests = [];
  const rest = selector.trim().replace(/\[([\w-]+)(?:="([^"]*)")?\]|([#.]?)([\w-]+)/g, (_all, attr, value, mark, name) => {
    if (attr) tests.push((e) => (value === undefined ? e.hasAttribute(attr) : e.getAttribute(attr) === value));
    else if (mark === "#") tests.push((e) => e.id === name);
    else if (mark === ".") tests.push((e) => e.classList.contains(name));
    else tests.push((e) => e.tagName === name.toUpperCase());
    return "";
  });
  if (rest || tests.length === 0) throw new Error(`fake-dom: unsupported selector ${selector}`);
  return (e) => tests.every((t) => t(e));
}

/** Compounds joined by spaces: each earlier one has to match some ancestor, in order. */
function matcher(selector) {
  const parts = selector.trim().split(/\s+/).map(compound);
  const last = parts.pop();
  return (e) => {
    if (!last(e)) return false;
    let at = e.parentNode;
    for (let i = parts.length - 1; i >= 0; i -= 1) {
      while (at && !(at instanceof FakeElement && parts[i](at))) at = at.parentNode;
      if (!at) return false;
      at = at.parentNode;
    }
    return true;
  };
}

class FakeElement extends FakeNode {
  #attrs = new Map();
  #listeners = new Map();
  value = "";

  constructor(tag) {
    super();
    this.tagName = String(tag).toUpperCase();
  }

  setAttribute(name, value) {
    this.#attrs.set(name, String(value));
  }

  getAttribute(name) {
    return this.#attrs.has(name) ? this.#attrs.get(name) : null;
  }

  hasAttribute(name) {
    return this.#attrs.has(name);
  }

  removeAttribute(name) {
    this.#attrs.delete(name);
  }

  #flag(name, on) {
    if (on) this.#attrs.set(name, "");
    else this.#attrs.delete(name);
  }

  get id() {
    return this.getAttribute("id") ?? "";
  }

  get hidden() {
    return this.hasAttribute("hidden");
  }

  set hidden(on) {
    this.#flag("hidden", on);
  }

  get disabled() {
    return this.hasAttribute("disabled");
  }

  set disabled(on) {
    this.#flag("disabled", on);
  }

  get open() {
    return this.hasAttribute("open");
  }

  set open(on) {
    this.#flag("open", on);
  }

  get classList() {
    const names = (this.getAttribute("class") ?? "").split(/\s+/).filter(Boolean);
    return { contains: (name) => names.includes(name) };
  }

  get children() {
    return this.childNodes.filter((node) => node instanceof FakeElement);
  }

  get firstElementChild() {
    return this.children[0] ?? null;
  }

  get textContent() {
    return this.childNodes.map((node) => node.textContent).join("");
  }

  set textContent(text) {
    this.replaceChildren(...(text === "" ? [] : [String(text)]));
  }

  append(...nodes) {
    for (const item of nodes) {
      const node = item instanceof FakeNode ? item : new FakeText(item);
      node.remove();
      node.parentNode = this;
      this.childNodes.push(node);
    }
  }

  replaceChildren(...nodes) {
    for (const node of [...this.childNodes]) node.remove();
    this.append(...nodes);
  }

  querySelectorAll(selector) {
    const matches = matcher(selector);
    const found = [];
    const walk = (node) => {
      for (const child of node.children) {
        if (matches(child)) found.push(child);
        walk(child);
      }
    };
    walk(this);
    return found;
  }

  querySelector(selector) {
    return this.querySelectorAll(selector)[0] ?? null;
  }

  addEventListener(type, handler) {
    if (!this.#listeners.has(type)) this.#listeners.set(type, []);
    this.#listeners.get(type).push(handler);
  }

  /** Runs the handlers; the promise settles when async ones are done. */
  dispatch(type) {
    const event = { type, target: this, isComposing: false };
    return Promise.all((this.#listeners.get(type) ?? []).map((handler) => handler(event)));
  }

  click() {
    return this.disabled ? Promise.resolve([]) : this.dispatch("click");
  }

  /** What typing does: set the value, then fire `input`. */
  type(text) {
    this.value = text;
    return this.dispatch("input");
  }

  focus() {
    if (this.isConnected && !this.hidden) document.activeElement = this;
  }
}

const document = {
  body: null,
  activeElement: null,
  createElement: (tag) => new FakeElement(tag),
  createTextNode: (data) => new FakeText(data),
  getElementById: (id) => document.body.querySelector(`#${id}`),
};

/**
 * Remembers how `names` are defined on `globalThis` right now, without reading
 * their values. The returned function puts every one back as it was, and
 * removes those that did not exist. All test files run in one process, so a
 * test that swaps a global has to call it when it ends.
 */
export function saveGlobals(names) {
  const saved = names.map((name) => [name, Object.getOwnPropertyDescriptor(globalThis, name)]);
  return () => {
    for (const [name, descriptor] of saved) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else delete globalThis[name];
    }
  };
}

/** The globals {@link installDom} replaces. */
export const DOM_GLOBALS = Object.freeze(["document", "Node"]);

/**
 * Puts a fresh `document` (and `Node`) on `globalThis`; returns the document.
 * It does not undo itself: take `saveGlobals(DOM_GLOBALS)` first and call it
 * when the test ends.
 */
export function installDom() {
  document.body = new FakeElement("body");
  document.activeElement = document.body;
  globalThis.Node = FakeNode;
  globalThis.document = document;
  return document;
}

/** The elements under `root` that are on screen: neither they nor an ancestor is `hidden`. */
export function visible(root, selector) {
  return root.querySelectorAll(selector).filter((node) => {
    for (let at = node; at && at !== root.parentNode; at = at.parentNode) if (at.hidden) return false;
    return true;
  });
}
