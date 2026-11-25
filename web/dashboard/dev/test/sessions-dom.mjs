// A small DOM stand-in for the session list render tests. It is not a test
// file (the `*.test.mjs` glob skips it) and models only what view-sessions.js
// and dom.js rely on: attributes mirrored by `id`, moving a node detaches it
// from its old parent, and detaching the focused node drops focus to the body.
// Anything else throws, so a test cannot pass on a silently empty match.

class FakeNode {
  constructor(doc) {
    this.ownerDocument = doc;
    this.parentNode = null;
    this.childNodes = [];
    /** How many times this node was taken out of a parent. */
    this.detachCount = 0;
  }

  get isConnected() {
    for (let node = this; node; node = node.parentNode) {
      if (node === this.ownerDocument.body) return true;
    }
    return false;
  }

  contains(other) {
    for (let node = other; node; node = node.parentNode) {
      if (node === this) return true;
    }
    return false;
  }
}

class FakeText extends FakeNode {
  constructor(doc, data) {
    super(doc);
    this.data = String(data);
  }

  get textContent() {
    return this.data;
  }
}

function detach(node) {
  const parent = node.parentNode;
  if (!parent) return;
  parent.childNodes.splice(parent.childNodes.indexOf(node), 1);
  node.parentNode = null;
  node.detachCount += 1;
  const doc = node.ownerDocument;
  if (node.contains(doc.activeElement)) doc.activeElement = doc.body;
}

class FakeElement extends FakeNode {
  constructor(doc, tagName) {
    super(doc);
    this.tagName = tagName.toUpperCase();
    this.attributes = new Map();
    this.listeners = new Map();
    this.disabled = false;
    this.ownValue = null;
    this.classList = {
      add: (name) => {
        const names = new Set((this.getAttribute("class") ?? "").split(/\s+/).filter(Boolean));
        names.add(name);
        this.setAttribute("class", [...names].join(" "));
      },
    };
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  getAttribute(name) {
    return this.attributes.has(name) ? this.attributes.get(name) : null;
  }

  get id() {
    return this.getAttribute("id") ?? "";
  }

  /** A `<select>` with nothing chosen yet reports its first option, as browsers do. */
  get value() {
    if (this.ownValue !== null) return this.ownValue;
    if (this.tagName === "SELECT") return this.children[0]?.getAttribute("value") ?? "";
    return "";
  }

  set value(text) {
    this.ownValue = String(text);
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
    this.replaceChildren(new FakeText(this.ownerDocument, text));
  }

  append(...nodes) {
    for (const node of nodes) {
      if (!(node instanceof FakeNode)) throw new TypeError("append: only nodes are supported");
      detach(node);
      node.parentNode = this;
      this.childNodes.push(node);
    }
  }

  replaceChildren(...nodes) {
    for (const child of [...this.childNodes]) detach(child);
    this.append(...nodes);
  }

  addEventListener(type, listener) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(listener);
  }

  focus() {
    if (this.isConnected) this.ownerDocument.activeElement = this;
  }

  closest() {
    throw new Error("closest is not supported by the DOM stand-in");
  }

  querySelector() {
    throw new Error("querySelector is not supported by the DOM stand-in");
  }

  querySelectorAll() {
    throw new Error("querySelectorAll is not supported by the DOM stand-in");
  }
}

class FakeDocument {
  constructor() {
    this.body = new FakeElement(this, "body");
    this.activeElement = this.body;
  }

  createElement(tagName) {
    return new FakeElement(this, tagName);
  }

  createTextNode(data) {
    return new FakeText(this, data);
  }

  getElementById(id) {
    return findAll(this.body, (node) => node.id === id)[0] ?? null;
  }
}

/** Every element under `root` (in document order) that `test` accepts. */
export function findAll(root, test) {
  const found = [];
  const walk = (node) => {
    for (const child of node.children) {
      if (test(child)) found.push(child);
      walk(child);
    }
  };
  walk(root);
  return found;
}

/** Dispatch a bubbling event of `type` on `target`. */
export function fire(target, type) {
  const event = { type, target, preventDefault() {} };
  for (let node = target; node; node = node.parentNode) {
    for (const listener of node.listeners?.get(type) ?? []) listener(event);
  }
}

/**
 * Install the stand-in on `globalThis` and count every use of the places a
 * choice could be persisted or sent. Returns the document, a mounted root and
 * the counters; call `restore()` when the test ends.
 */
export function installDom() {
  const names = ["document", "Node", "localStorage", "sessionStorage", "location", "history", "fetch"];
  const saved = names.map((name) => [name, Object.getOwnPropertyDescriptor(globalThis, name)]);
  const doc = new FakeDocument();
  const touched = { localStorage: 0, sessionStorage: 0, location: 0, history: 0, fetch: 0 };
  const define = (name, descriptor) =>
    Object.defineProperty(globalThis, name, { configurable: true, ...descriptor });

  define("document", { value: doc });
  define("Node", { value: FakeNode });
  for (const name of Object.keys(touched)) {
    define(name, {
      get() {
        touched[name] += 1;
        throw new Error(`${name} must not be used by the session list`);
      },
    });
  }

  const root = doc.createElement("div");
  doc.body.append(root);
  return {
    document: doc,
    root,
    touched,
    restore() {
      for (const [name, descriptor] of saved) {
        if (descriptor) Object.defineProperty(globalThis, name, descriptor);
        else delete globalThis[name];
      }
    },
  };
}
