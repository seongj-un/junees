// Building the page without innerHTML: every server value goes in as text.

import { formatDateTime, formatRelative } from "./format.js";

const SVG_NS = "http://www.w3.org/2000/svg";

function apply(node, attrs) {
  for (const [key, value] of Object.entries(attrs ?? {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") node.setAttribute("class", value);
    else if (key === "text") node.textContent = String(value);
    else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2), value);
    } else node.setAttribute(key, value === true ? "" : String(value));
  }
}

function append(node, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
}

/** `el("p", { class: "x" }, "text", otherNode)`. Strings become text nodes. */
export function el(tag, attrs, ...children) {
  const node = document.createElement(tag);
  apply(node, attrs);
  append(node, children);
  return node;
}

/** The same as {@link el} for SVG elements. */
export function svg(tag, attrs, ...children) {
  const node = document.createElementNS(SVG_NS, tag);
  apply(node, attrs);
  append(node, children);
  return node;
}

/** A `<time>` showing `label`, with the exact local time on hover. */
export function timeEl(date, label) {
  if (!date) return el("span", { class: "muted" }, "-");
  return el(
    "time",
    { datetime: date.toISOString(), title: date.toLocaleString("ko-KR") },
    label,
  );
}

/** Relative time on top, the exact time under it; `-` when missing. */
export function whenCell(date, now, empty = "-") {
  if (!date) return el("span", { class: "muted" }, empty);
  return [
    timeEl(date, formatRelative(date, now)),
    el("span", { class: "row-sub" }, formatDateTime(date, now)),
  ];
}

/** A file path in the code font that wraps after a `/` before anywhere else. */
export function pathCode(path) {
  if (!path) return el("span", { class: "muted" }, "-");
  const parts = String(path).split("/");
  return el(
    "code",
    { class: "code" },
    parts.map((part, i) => (i < parts.length - 1 ? [part, "/", el("wbr")] : part)),
  );
}

/** A small text badge; `tone` is ok, warn, fail, info or neutral. */
export function badge(text, tone = "neutral") {
  return el("span", { class: `badge badge-${tone}` }, text);
}

/** A command or name in the code font. */
export function code(text) {
  return el("code", { class: "code" }, text ?? "-");
}

let cardCount = 0;

/** A card with an optional heading. */
export function card(title, ...children) {
  cardCount += 1;
  const id = title ? `card-${cardCount}` : null;
  return el(
    "section",
    { class: "card", "aria-labelledby": id },
    title ? el("h2", { class: "card-title", id }, title) : null,
    ...children,
  );
}

/**
 * A table that turns into one card per row on narrow screens. `columns`
 * is `[{ label, cell(row) -> Node|string, className? }]`.
 */
export function dataTable({ caption, columns, rows, rowKey }) {
  const head = el(
    "thead",
    null,
    el(
      "tr",
      null,
      columns.map((col) => el("th", { scope: "col", class: col.className }, col.label)),
    ),
  );
  const body = el(
    "tbody",
    null,
    rows.map((row) => tableRow(columns, row, rowKey)),
  );
  return el(
    "div",
    { class: "table-wrap" },
    el("table", { class: "data-table" }, el("caption", { class: "sr-only" }, caption), head, body),
  );
}

/** One `<tr>` of {@link dataTable}, also used to redraw a single row. */
export function tableRow(columns, row, rowKey) {
  return el(
    "tr",
    { "data-key": rowKey ? rowKey(row) : null },
    columns.map((col, index) =>
      el(
        index === 0 ? "th" : "td",
        { class: col.className, scope: index === 0 ? "row" : null },
        index === 0 ? null : el("span", { class: "cell-label", "aria-hidden": "true" }, col.label),
        el("div", { class: "cell-value" }, col.cell(row)),
      ),
    ),
  );
}

/** A centered message for a screen with nothing to show yet. */
export function notice({ title, body, tone = "neutral", action }) {
  return el(
    "div",
    { class: `notice notice-${tone}` },
    el("p", { class: "notice-title" }, title),
    body ? el("p", { class: "notice-body" }, body) : null,
    action ?? null,
  );
}

/** Placeholder rows while the first answer is on its way. */
export function skeleton(lines = 4) {
  return el(
    "div",
    { class: "skeleton", "aria-hidden": "true" },
    Array.from({ length: lines }, (_, i) => el("div", { class: `skeleton-line w${(i % 3) + 1}` })),
  );
}
