// 세션: the most recently changed sessions.

import { code, dataTable, el, notice, pathCode, whenCell } from "./dom.js";
import { formatInt, formatUsd, parseTime } from "./format.js";
import { SESSION_ID, sessionHash } from "./routes.js";

export const LIMIT = 50;

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

export function render(root, data, { now }) {
  const sessions = Array.isArray(data.sessions) ? data.sessions : [];
  if (sessions.length === 0) {
    root.replaceChildren(
      notice({
        title: "아직 저장된 세션이 없습니다",
        body: [code("junees"), "로 대화를 시작하면 여기에 최근 세션이 나타납니다."],
      }),
    );
    return;
  }

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

  root.replaceChildren(
    el(
      "p",
      { class: "summary" },
      sessions.length >= LIMIT
        ? `최근에 바뀐 세션 ${formatInt(sessions.length)}개를 보여 줍니다.`
        : `세션 ${formatInt(sessions.length)}개`,
    ),
    table,
  );
}
