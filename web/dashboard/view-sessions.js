// 세션: the most recently changed sessions.

import { code, dataTable, el, notice, pathCode, whenCell } from "./dom.js";
import { formatInt, formatUsd, parseTime } from "./format.js";
import { SESSION_ID, sessionHash } from "./routes.js";

export const LIMIT = 50;

const searchText = (text) => (text ?? "").normalize("NFC").toLowerCase();

/** Filter only the loaded list, leaving its original order intact. */
export function filterSessions(sessions, query) {
  const needle = searchText(query.trim());
  if (!needle) return sessions;
  return sessions.filter(
    (session) => searchText(session.title).includes(needle) || searchText(session.cwd).includes(needle),
  );
}

/** The first entry is the default: the order the server sent, newest first. */
export const SORTS = Object.freeze([
  Object.freeze({ value: "recent", label: "최근 순" }),
  Object.freeze({ value: "cost", label: "비용 큰 순" }),
  Object.freeze({ value: "messages", label: "메시지 많은 순" }),
]);

// No prototype, so a name like "constructor" is not taken for a sort.
const SORT_FIELDS = Object.freeze({ __proto__: null, cost: "cost_usd", messages: "messages" });

/**
 * Order only the loaded list, without changing the array passed in. The
 * largest value comes first and equal values keep the server order. A session
 * whose value is unknown (`null` cost) goes last instead of counting as 0.
 */
export function sortSessions(sessions, sort) {
  const field = SORT_FIELDS[sort];
  if (!field) return sessions;
  const known = (session) => typeof session[field] === "number" && Number.isFinite(session[field]);
  // Array.prototype.sort is stable, so ties stay in server order.
  return sessions.slice().sort((a, b) => {
    if (!known(a) || !known(b)) return Number(known(b)) - Number(known(a));
    return b[field] - a[field];
  });
}

/**
 * The preview without the title it starts with: the server takes the title
 * from the first line of the same message, so showing both repeats it.
 */
export function previewAfterTitle(title, preview) {
  const text = (preview ?? "").trim();
  const head = (title ?? "").trim();
  if (!head || !text.startsWith(head)) return text;
  return text.slice(head.length).trim();
}

function titleCell(session) {
  const rest = previewAfterTitle(session.title, session.preview);
  const title = session.title || "제목 없음";
  // Only ids in the server's own format become links.
  const linked = typeof session.id === "string" && SESSION_ID.test(session.id);
  return [
    linked
      ? el(
          "a",
          {
            class: "row-title row-link",
            href: sessionHash(session.id),
            // An id lets focus come back here, and survive a refresh.
            id: `session-link-${session.id}`,
            "data-session-link": session.id,
          },
          title,
        )
      : el("span", { class: "row-title" }, title),
    rest ? el("span", { class: "row-sub clamp-2" }, rest) : null,
  ];
}

/** A click anywhere on a row opens the session, like its title link. */
function openOnRowClick(table) {
  table.addEventListener("click", (event) => {
    if (event.target.closest("a, button, summary")) return;
    if (window.getSelection()?.toString()) return;
    const link = event.target.closest("tr")?.querySelector("[data-session-link]");
    if (link) link.click();
  });
}

function tokensCell(session) {
  const input = session.input_tokens ?? 0;
  const output = session.output_tokens ?? 0;
  return [
    el("span", { class: "num" }, formatInt(input + output)),
    el("span", { class: "row-sub num" }, `입력 ${formatInt(input)} · 출력 ${formatInt(output)}`),
  ];
}

function sessionsTable(sessions, now) {
  const table = dataTable({
    caption: "최근 세션. 제목을 누르면 대화를 엽니다.",
    rowKey: (s) => s.id,
    rows: sessions,
    columns: [
      { label: "세션", cell: titleCell, className: "col-main col-wide" },
      { label: "폴더", cell: (s) => pathCode(s.cwd), className: "col-path" },
      {
        label: "갱신",
        cell: (s) => whenCell(parseTime(s.updated_at), now),
        className: "col-time",
      },
      { label: "메시지", cell: (s) => el("span", { class: "num" }, formatInt(s.messages)), className: "col-num" },
      { label: "토큰", cell: tokensCell, className: "col-num" },
      { label: "비용", cell: (s) => el("span", { class: "num" }, formatUsd(s.cost_usd)), className: "col-num" },
    ],
  });
  table.classList.add("table-clickable");
  openOnRowClick(table);
  return table;
}

// Only a still-mounted filter is reused. Leaving the screen drops the query.
const mountedFilters = new WeakMap();

function mountFilter(root) {
  let sessions;
  let now;
  const input = el("input", {
    id: "session-search",
    type: "search",
    class: "session-filter-input",
    autocomplete: "off",
    spellcheck: "false",
    "aria-describedby": "session-search-scope",
  });
  const clear = el("button", { type: "button", class: "button button-secondary" }, "지우기");
  // The choice lives only in this control: no URL, storage or request carries it.
  const sort = el(
    "select",
    { id: "session-sort", class: "session-sort-select", "aria-describedby": "session-search-scope" },
    SORTS.map((option) => el("option", { value: option.value }, option.label)),
  );
  const summary = el("p", { class: "summary", role: "status" });
  const results = el("div");

  function paintResults() {
    const query = input.value.trim();
    const matches = sortSessions(filterSessions(sessions, query), sort.value);
    const total = formatInt(sessions.length);
    const message = query
      ? `검색 결과 ${formatInt(matches.length)}개 / 불러온 세션 ${total}개`
      : sessions.length >= LIMIT
        ? `최근에 바뀐 세션 ${total}개를 보여 줍니다.`
        : `세션 ${total}개`;
    if (summary.textContent !== message) summary.textContent = message;
    clear.disabled = !input.value;

    const active = document.activeElement;
    const focusId = results.contains(active) ? active.id : null;
    if (sessions.length === 0) {
      results.replaceChildren(
        notice({
          title: "아직 저장된 세션이 없습니다",
          body: [code("junees"), "로 대화를 시작하면 여기에 최근 세션이 나타납니다."],
        }),
      );
    } else if (matches.length === 0) {
      results.replaceChildren(
        notice({
          title: "검색 결과가 없습니다",
          body: "다른 검색어를 입력하거나 검색어를 지워 보세요.",
        }),
      );
    } else {
      results.replaceChildren(sessionsTable(matches, now));
    }
    if (focusId) (document.getElementById(focusId) ?? input).focus({ preventScroll: true });
  }

  input.addEventListener("input", paintResults);
  sort.addEventListener("change", paintResults);
  clear.addEventListener("click", () => {
    input.value = "";
    input.focus();
    paintResults();
  });

  const filter = el(
    "div",
    { class: "session-filter" },
    el("label", { for: input.id }, "제목 또는 작업 폴더 검색"),
    el("div", { class: "session-filter-fields" }, input, clear),
    el("div", { class: "session-sort" }, el("label", { for: sort.id }, "정렬"), sort),
    el(
      "p",
      { class: "summary", id: "session-search-scope" },
      `이미 불러온 최근 세션 최대 ${LIMIT}개에서만 검색하고 정렬합니다.`,
    ),
  );
  const update = (data, at) => {
    sessions = data;
    now = at;
    paintResults();
  };
  mountedFilters.set(filter, update);
  root.replaceChildren(filter, summary, results);
  return update;
}

export function render(root, data, { now }) {
  // Keep the input and the sort control themselves across refreshes, so the
  // caret, IME state and chosen order survive.
  const update = mountedFilters.get(root.firstElementChild) ?? mountFilter(root);
  update(Array.isArray(data.sessions) ? data.sessions : [], now);
}
