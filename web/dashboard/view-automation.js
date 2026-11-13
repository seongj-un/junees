// 예약 작업 and 웹훅: whether each one runs, when, and how it last went,
// with a pause/resume button per item.

import { ACTION_TEXT, TARGETS, actionFor, runAction } from "./actions.js";
import { badge, code, dataTable, el, notice, pathCode, tableRow, whenCell } from "./dom.js";
import { formatInt, parseTime } from "./format.js";

const RUN_STATUS = {
  ok: ["성공", "ok"],
  failed: ["실패", "fail"],
  running: ["실행 중", "info"],
  // A one-time job whose time passed while nothing was running to start it.
  missed: ["놓침", "warn"],
};
// The request format a route accepts (headers and signature), not where it comes from.
const KINDS = { github: "GitHub 형식", generic: "일반 형식" };

function lastRunCell(lastRun, now) {
  if (!lastRun) return el("span", { class: "muted" }, "아직 실행하지 않음");
  const [label, tone] = RUN_STATUS[lastRun.status] ?? [lastRun.status ?? "알 수 없음", "neutral"];
  const at = parseTime(lastRun.at);
  return [
    el("span", { class: "inline" }, badge(label, tone), at ? whenCell(at, now)[0] : null),
    lastRun.note ? el("span", { class: "row-sub" }, lastRun.note) : null,
  ];
}

function stateCell(item, command, button) {
  const parts = [];
  if (item.paused_for_home) parts.push(badge("멈춤: 홈 폴더", "warn"));
  else if (item.paused) parts.push(badge("멈춤", "neutral"));
  else parts.push(badge("켜짐", "ok"));
  if (item.allow_home) parts.push(badge("홈 폴더 허용", "info"));
  return [
    el("span", { class: "inline" }, parts),
    item.paused_for_home
      ? el(
          "span",
          { class: "row-sub" },
          "작업 폴더가 홈이라 멈췄습니다. 홈에서 돌려도 되면 터미널에서 다음 명령으로 다시 켭니다.",
          el("span", { class: "command" }, code(command)),
        )
      : null,
    button,
  ];
}

function nameCell(item, sub) {
  return [el("span", { class: "row-title" }, item.name || item.id || "-"), sub ? el("span", { class: "row-sub" }, sub) : null];
}

function summary(items, noun) {
  const paused = items.filter((item) => item.paused || item.paused_for_home).length;
  const text = `${noun} ${formatInt(items.length)}개`;
  return el("p", { class: "summary", "data-summary": "" }, paused ? `${text} · 멈춤 ${formatInt(paused)}개` : text);
}

/**
 * A list with a pause/resume button per item. A successful action redraws
 * only that item's row (and the count above the table) from the server's
 * answer, and keeps the cached list in step for the next full render.
 */
function renderList(root, data, ctx, { target, field, noun, caption, columns }) {
  const t = TARGETS[target];
  const items = Array.isArray(data[field]) ? data[field] : [];

  const actionButton = (item) => {
    const action = actionFor(item);
    if (!action) return null;
    const button = el(
      "button",
      {
        type: "button",
        class: "button button-secondary button-small row-action",
        // An id keeps focus on the button across the 30-second refresh.
        id: `action-${target}-${t.key(item)}`,
        "data-action-key": t.key(item),
        "aria-label": `${t.label(item)} ${t.noun} ${ACTION_TEXT[action].verb}`,
      },
      ACTION_TEXT[action].verb,
    );
    button.addEventListener("click", () => act(item, action, button));
    return button;
  };

  const cols = columns(ctx.now, actionButton);
  const rowKey = (item) => t.key(item);

  const act = async (item, action, button) => {
    const result = await runAction({ target, item, action, opener: button });
    if (result.outcome === "lock") return ctx.lock();
    if (result.outcome === "reload") return ctx.reload();
    if (result.outcome === "failed") {
      ctx.announce(result.message);
      button.focus();
      return;
    }
    if (result.outcome !== "done") {
      button.focus();
      return;
    }
    const index = items.findIndex((other) => t.key(other) === t.key(item));
    const changed = result.item && typeof result.item === "object" ? result.item : item;
    if (index >= 0) items[index] = changed;
    const key = CSS.escape(String(t.key(item)));
    root.querySelector(`tr[data-key="${key}"]`)?.replaceWith(tableRow(cols, changed, rowKey));
    root.querySelector("[data-summary]")?.replaceWith(summary(items, noun));
    const next = root.querySelector(`[data-action-key="${key}"]`);
    (next ?? root.querySelector(`tr[data-key="${key}"] th`))?.focus();
    ctx.announce(`${t.label(item)} ${t.object} ${ACTION_TEXT[action].done}.`);
  };

  root.replaceChildren(summary(items, noun), dataTable({ caption, rowKey, rows: items, columns: cols }));
}

export function renderCron(root, data, ctx) {
  const jobs = Array.isArray(data.jobs) ? data.jobs : [];
  if (jobs.length === 0) {
    root.replaceChildren(
      notice({
        title: "등록된 예약 작업이 없습니다",
        body: [code("junees cron add"), "로 정해진 시각에 돌 작업을 추가할 수 있습니다."],
      }),
    );
    return;
  }
  renderList(root, data, ctx, {
    target: "cron",
    field: "jobs",
    noun: "예약 작업",
    caption: "예약 작업",
    columns: (now, button) => [
      { label: "작업", cell: (job) => nameCell(job, job.id && job.name ? `ID ${job.id}` : null), className: "col-main" },
      { label: "일정", cell: (job) => code(job.schedule), className: "col-nowrap" },
      { label: "폴더", cell: (job) => pathCode(job.dir), className: "col-path" },
      {
        label: "상태",
        className: "col-state",
        cell: (job) => stateCell(job, `junees cron allow-home ${job.id || job.name}`, button(job)),
      },
      {
        label: "다음 실행",
        className: "col-time",
        // A paused job keeps its next time on the server but does not run.
        cell: (job) =>
          job.paused || job.paused_for_home
            ? el("span", { class: "muted" }, "멈춰 있음")
            : whenCell(parseTime(job.next_run), now, "예정 없음"),
      },
      { label: "마지막 실행", cell: (job) => lastRunCell(job.last_run, now), className: "col-last" },
    ],
  });
}

export function renderWebhooks(root, data, ctx) {
  const routes = Array.isArray(data.routes) ? data.routes : [];
  if (routes.length === 0) {
    root.replaceChildren(
      notice({
        title: "등록된 웹훅 경로가 없습니다",
        body: [code("junees webhook add"), "로 외부 요청을 받아 실행할 경로를 추가할 수 있습니다."],
      }),
    );
    return;
  }
  renderList(root, data, ctx, {
    target: "webhooks",
    field: "routes",
    noun: "웹훅 경로",
    caption: "웹훅 경로",
    columns: (now, button) => [
      { label: "경로", cell: (route) => nameCell(route, KINDS[route.kind] ?? route.kind), className: "col-main" },
      { label: "폴더", cell: (route) => pathCode(route.dir), className: "col-path" },
      {
        label: "상태",
        className: "col-state",
        cell: (route) => stateCell(route, `junees webhook allow-home ${route.name}`, button(route)),
      },
      {
        label: "시크릿",
        cell: (route) => (route.has_secret ? badge("있음", "ok") : badge("없음", "warn")),
      },
      { label: "마지막 실행", cell: (route) => lastRunCell(route.last_run, now), className: "col-last" },
    ],
  });
}
