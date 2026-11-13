// 점검: the offline `junees doctor` checks, grouped by area.

import { badge, card, el, notice } from "./dom.js";
import { formatInt } from "./format.js";

const AREAS = {
  "state folder": "상태 폴더",
  settings: "설정",
  keys: "API 키",
  "claude-code": "Claude Code",
  programs: "프로그램",
  sandbox: "샌드박스",
  browser: "브라우저",
  discord: "디스코드",
  telegram: "텔레그램",
  slack: "Slack",
  cron: "예약 작업",
  webhooks: "웹훅",
  sessions: "세션",
  mcp: "MCP 서버",
  plugins: "플러그인",
  online: "온라인 확인",
};
const STATUS = { ok: ["정상", "ok"], warn: ["주의", "warn"], fail: ["실패", "fail"] };

/** Area name as shown; unknown areas keep the server's name. */
export function areaLabel(name) {
  return AREAS[name] ?? name ?? "기타";
}

let onlyProblems = false;

function checkItem(check) {
  const [label, tone] = STATUS[check.status] ?? [check.status ?? "알 수 없음", "neutral"];
  return el(
    "li",
    { class: `check check-${tone}` },
    badge(label, tone),
    el(
      "div",
      { class: "check-body" },
      el("p", { class: "check-name" }, check.name ?? "-"),
      check.detail ? el("p", { class: "check-detail" }, check.detail) : null,
      check.fix ? el("p", { class: "check-fix" }, el("span", { class: "check-fix-label" }, "고치는 법"), check.fix) : null,
    ),
  );
}

export function render(root, data, ctx) {
  const sections = (Array.isArray(data.sections) ? data.sections : []).map((section) => ({
    ...section,
    checks: Array.isArray(section.checks) ? section.checks : [],
  }));
  const all = sections.flatMap((section) => section.checks);
  if (all.length === 0) {
    root.replaceChildren(notice({ title: "점검 결과가 없습니다", body: "서버가 점검 항목을 하나도 보내지 않았습니다." }));
    return;
  }

  const count = (status) => all.filter((check) => check.status === status).length;
  const fails = count("fail");
  const warns = count("warn");
  const problems = fails + warns;
  const show = onlyProblems && problems > 0;

  const toggle = el("input", {
    type: "checkbox",
    id: "doctor-only-problems",
    checked: show,
    disabled: problems === 0,
    onchange: (event) => {
      onlyProblems = event.target.checked;
      render(root, data, ctx);
      root.querySelector("#doctor-only-problems")?.focus();
    },
  });

  const visible = sections
    .map((section) => ({
      ...section,
      checks: show ? section.checks.filter((check) => check.status !== "ok") : section.checks,
    }))
    .filter((section) => section.checks.length > 0);

  root.replaceChildren(
    el(
      "div",
      { class: "doctor-bar" },
      el(
        "p",
        { class: "summary" },
        problems === 0
          ? `${formatInt(all.length)}개 항목 모두 정상입니다.`
          : `${formatInt(all.length)}개 항목 중 실패 ${formatInt(fails)}개, 주의 ${formatInt(warns)}개`,
      ),
      el(
        "span",
        { class: "check-toggle" },
        toggle,
        el("label", { for: "doctor-only-problems" }, "문제만 보기"),
      ),
    ),
    el(
      "div",
      { class: "masonry" },
      visible.map((section) => card(areaLabel(section.name), el("ul", { class: "checks" }, section.checks.map(checkItem)))),
    ),
  );
}
