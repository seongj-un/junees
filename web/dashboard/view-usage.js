// 사용량: cost per day as bars and totals per model.

import { card, code, dataTable, el, notice } from "./dom.js";
import { formatCompact, formatDay, formatDayShort, formatInt, formatUsd } from "./format.js";
import { costLabel, dailyTotals, hasUnpriced, localDay, modelTotals, niceCeil } from "./usage.js";

export const PERIODS = [7, 30, 90];
export const DEFAULT_DAYS = 30;

/** The 7/30/90-day choice; it stays on screen while the data reloads. */
export function renderControls(root, { params, setParams }) {
  const name = "usage-period";
  root.replaceChildren(
    el(
      "fieldset",
      { class: "segmented" },
      el("legend", { class: "sr-only" }, "기간"),
      PERIODS.map((days) => {
        const id = `${name}-${days}`;
        return el(
          "span",
          { class: "segmented-item" },
          el("input", {
            type: "radio",
            name,
            id,
            value: String(days),
            checked: params.days === days,
            onchange: () => setParams({ days }),
          }),
          el("label", { for: id }, `최근 ${days}일`),
        );
      }),
    ),
  );
}

function stat(label, value, sub, accent = false) {
  return el(
    "div",
    { class: `stat${accent ? " stat-accent" : ""}` },
    el("p", { class: "stat-label" }, label),
    el("p", { class: "stat-value num" }, value),
    sub ? el("p", { class: "stat-sub" }, sub) : null,
  );
}

const axisFormat = new Intl.NumberFormat("en-US", { maximumSignificantDigits: 3 });

/** Axis labels keep small amounts readable (`$0.005`), unlike {@link formatUsd}. */
function axisUsd(value) {
  return `$${axisFormat.format(value)}`;
}

function costChart(days, count) {
  const max = days.reduce((m, d) => Math.max(m, d.cost), 0);
  if (max === 0) {
    return el(
      "p",
      { class: "chart-empty" },
      days.some((d) => d.unpriced)
        ? "이 기간의 사용은 가격을 알 수 없어(구독 사용이나 가격표에 없는 모델) 비용 막대를 그리지 않았습니다. 토큰 사용량은 아래 모델별 표에서 볼 수 있습니다."
        : "이 기간에는 비용이 기록되지 않았습니다. 토큰 사용량은 아래 모델별 표에서 볼 수 있습니다.",
    );
  }
  const top = niceCeil(max);
  const peak = days.reduce((a, b) => (b.cost > a.cost ? b : a));
  const total = days.reduce((sum, d) => sum + d.cost, 0);

  const readout = el("p", { class: "chart-readout", "aria-hidden": "true" });
  const showDay = (day) => {
    readout.replaceChildren(
      el("span", { class: "chart-readout-day" }, formatDay(day.date)),
      el("span", { class: "num" }, costLabel(day.cost, day.unpriced)),
      el("span", { class: "muted num" }, `토큰 ${formatInt(day.input + day.output)}`),
    );
  };
  showDay(peak);

  const bars = days.map((day) => {
    const bar = el("div", {
      class: `bar${day.cost === 0 ? " bar-zero" : ""}`,
      title: `${formatDay(day.date)} ${costLabel(day.cost, day.unpriced)}`,
    });
    bar.style.setProperty("--v", String(day.cost / top));
    bar.addEventListener("pointerenter", () => showDay(day));
    return bar;
  });
  const middle = days[Math.floor((days.length - 1) / 2)];

  return el(
    "figure",
    { class: "chart" },
    readout,
    el(
      "div",
      {
        class: "chart-plot",
        role: "img",
        "aria-label": `최근 ${count}일 날짜별 비용 막대 그래프. 합계 ${formatUsd(total)}, 가장 많이 쓴 날은 ${formatDay(peak.date)} ${formatUsd(peak.cost)}입니다.`,
      },
      el(
        "div",
        { class: "chart-axis", "aria-hidden": "true" },
        el("span", null, axisUsd(top)),
        el("span", null, axisUsd(top / 2)),
        el("span", null, "$0"),
      ),
      el(
        "div",
        { class: `chart-bars${days.length > 45 ? " chart-bars-dense" : ""}` },
        bars,
      ),
      el(
        "div",
        { class: "chart-x", "aria-hidden": "true" },
        el("span", null, formatDayShort(days[0].date)),
        el("span", null, formatDayShort(middle.date)),
        el("span", null, formatDayShort(days[days.length - 1].date)),
      ),
    ),
    el(
      "details",
      { class: "chart-table", id: "usage-day-table" },
      el("summary", null, "날짜별 금액을 표로 보기"),
      el(
        "div",
        { class: "table-wrap" },
        el(
          "table",
          { class: "plain-table" },
          el("caption", { class: "sr-only" }, `최근 ${count}일 날짜별 비용`),
          el("thead", null, el("tr", null, el("th", { scope: "col" }, "날짜"), el("th", { scope: "col", class: "col-num" }, "토큰"), el("th", { scope: "col", class: "col-num" }, "비용"))),
          el(
            "tbody",
            null,
            [...days].reverse().map((day) =>
              el(
                "tr",
                null,
                el("th", { scope: "row" }, formatDay(day.date)),
                el("td", { class: "col-num num" }, formatInt(day.input + day.output)),
                el("td", { class: "col-num num" }, costLabel(day.cost, day.unpriced)),
              ),
            ),
          ),
        ),
      ),
    ),
  );
}

export function render(root, data, { params, now }) {
  const rows = Array.isArray(data.days) ? data.days : [];
  const count = params.days;
  if (rows.length === 0) {
    root.replaceChildren(
      notice({
        title: `최근 ${count}일 동안 사용 기록이 없습니다`,
        body: "대화나 예약 작업이 모델을 부르면 날짜별로 여기에 쌓입니다.",
      }),
    );
    return;
  }

  const days = dailyTotals(rows, count, localDay(now));
  const models = modelTotals(rows);
  const total = data.total ?? {};
  const cacheRead = models.reduce((sum, m) => sum + m.cacheRead, 0);

  root.replaceChildren(
    el(
      "div",
      { class: "stats" },
      stat(
        "총 비용",
        typeof total.cost_usd === "number" ? formatUsd(total.cost_usd) : "가격 없음",
        hasUnpriced(rows) && typeof total.cost_usd === "number"
          ? `최근 ${count}일, 가격을 모르는 사용은 빼고 셈`
          : `최근 ${count}일`,
        true,
      ),
      stat("입력 토큰", formatCompact(total.input_tokens), cacheRead ? `캐시 읽기 ${formatCompact(cacheRead)}` : null),
      stat("출력 토큰", formatCompact(total.output_tokens)),
    ),
    card("날짜별 비용", costChart(days, count)),
    card(
      "모델별 합계",
      dataTable({
        caption: `최근 ${count}일 모델별 합계`,
        rowKey: (m) => m.model,
        rows: models,
        columns: [
          { label: "모델", cell: (m) => code(m.model), className: "col-main" },
          { label: "입력", cell: (m) => el("span", { class: "num" }, formatInt(m.input)), className: "col-num" },
          { label: "출력", cell: (m) => el("span", { class: "num" }, formatInt(m.output)), className: "col-num" },
          { label: "캐시 읽기", cell: (m) => el("span", { class: "num" }, formatInt(m.cacheRead)), className: "col-num" },
          { label: "캐시 쓰기", cell: (m) => el("span", { class: "num" }, formatInt(m.cacheWrite)), className: "col-num" },
          { label: "비용", cell: (m) => el("span", { class: "num" }, costLabel(m.cost, m.unpriced)), className: "col-num" },
        ],
      }),
    ),
  );
}
