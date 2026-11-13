// 상태: version, backend, keys and the sandbox.

import { badge, card, code, el, pathCode } from "./dom.js";
import { formatDuration } from "./format.js";

const BACKENDS = { api: "API 직접 호출", "claude-code": "Claude Code" };
const PROVIDERS = { anthropic: "Anthropic", openai: "OpenAI 호환" };
const KEY_NAMES = { anthropic: "Anthropic", openai: "OpenAI" };
const SOURCES = { env: "환경 변수", "config.toml": "config.toml" };

function row(label, value) {
  return [el("dt", null, label), el("dd", null, value)];
}

function keyRow(name, key) {
  const set = Boolean(key?.set);
  const source = key?.source ? (SOURCES[key.source] ?? key.source) : null;
  return row(
    KEY_NAMES[name] ?? name,
    el(
      "span",
      { class: "inline" },
      badge(set ? "있음" : "없음", set ? "ok" : "neutral"),
      set && source ? el("span", { class: "muted" }, `${source}에서 읽음`) : null,
    ),
  );
}

export function render(root, status) {
  const keys = status.keys ?? {};
  const needsKey = status.backend === "api" && status.provider && keys[status.provider]?.set === false;
  const sandbox = status.sandbox ?? {};

  root.replaceChildren(
    el(
      "div",
      { class: "grid grid-3" },
      card(
        "실행 환경",
        el(
          "dl",
          { class: "facts" },
          row("버전", status.version ?? "-"),
          row("백엔드", BACKENDS[status.backend] ?? status.backend ?? "-"),
          row("Provider", PROVIDERS[status.provider] ?? status.provider ?? "-"),
          row("모델", status.model ? code(status.model) : "-"),
          row("상태 폴더", pathCode(status.home)),
          row("대시보드 실행 시간", formatDuration(status.uptime_secs)),
        ),
      ),
      card(
        "API 키",
        el(
          "dl",
          { class: "facts" },
          Object.keys({ anthropic: 1, openai: 1, ...keys }).map((name) => keyRow(name, keys[name])),
        ),
        needsKey
          ? el(
              "p",
              { class: "callout callout-warn" },
              `지금 쓰는 provider(${PROVIDERS[status.provider] ?? status.provider})의 키가 없습니다. 대화를 시작하면 인증 오류가 납니다.`,
            )
          : el("p", { class: "hint" }, "키 값은 대시보드에 보내지 않고, 있는지와 읽은 곳만 보여 줍니다."),
      ),
      card(
        "샌드박스",
        el(
          "dl",
          { class: "facts" },
          row(
            "Landlock",
            sandbox.landlock
              ? el(
                  "span",
                  { class: "inline" },
                  badge("사용 가능", "ok"),
                  typeof sandbox.abi === "number" ? el("span", { class: "muted" }, `ABI ${sandbox.abi}`) : null,
                )
              : badge("사용할 수 없음", "warn"),
          ),
        ),
        el(
          "p",
          { class: "hint" },
          sandbox.landlock
            ? "bash 도구가 실행하는 명령은 커널 격리 안에서 돕니다."
            : "bash 도구의 명령을 격리하지 못합니다. 예약 작업과 웹훅의 bash 명령은 기본 설정에서 거절됩니다.",
        ),
      ),
    ),
  );
}
