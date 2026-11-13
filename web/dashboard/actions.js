// Pausing and resuming cron jobs and webhook routes: the confirmation
// dialog, the POST and what to say when it fails.

import { ApiError, describeError, postJson } from "./api.js";
import { el } from "./dom.js";

/** What each list calls its items and where their actions live. */
export const TARGETS = {
  cron: {
    noun: "예약 작업",
    object: "예약 작업을",
    key: (item) => item.id,
    label: (item) => item.name || item.id,
    path: (item) => `/api/cron/${encodeURIComponent(item.id)}`,
    pausedNote: "다시 켤 때까지 일정이 돌아와도 실행하지 않습니다.",
    resumedNote: "다음 일정부터 다시 실행합니다.",
  },
  webhooks: {
    noun: "웹훅 경로",
    object: "웹훅 경로를",
    key: (item) => item.name,
    label: (item) => item.name,
    path: (item) => `/api/webhooks/${encodeURIComponent(item.name)}`,
    pausedNote: "다시 켤 때까지 이 경로로 오는 요청은 실행하지 않고 503으로 답합니다.",
    resumedNote: "이 경로로 오는 요청을 다시 받아 실행합니다.",
  },
};

/**
 * The action an item offers: `pause`, `resume`, or `null` for one paused
 * because it works in the home folder. Resuming that one fails until
 * `allow-home`, which the dashboard does not do, so the list shows the
 * command instead of a button that cannot work.
 */
export function actionFor(item) {
  if (item.paused_for_home) return null;
  return item.paused ? "resume" : "pause";
}

export const ACTION_TEXT = {
  pause: { verb: "멈추기", busy: "멈추는 중", done: "멈췄습니다", ask: "멈출까요?" },
  resume: { verb: "다시 켜기", busy: "켜는 중", done: "다시 켰습니다", ask: "다시 켤까요?" },
};

/**
 * What to tell the user about a failed action: `{ text, detail, outcome,
 * retry }`. `detail` is the server's own message, shown as it is (409).
 * `outcome` is what the list does after the dialog closes: `stay`,
 * `reload` (the item is gone, changed, or the result is unknown) or `lock`
 * (the token is no longer accepted).
 * `retry` says whether sending the same request again can help.
 */
export function describeActionError(err) {
  if (err instanceof ApiError) {
    if (err.kind === "unauthorized") {
      return {
        text: "토큰이 맞지 않아 바꾸지 못했습니다. 대시보드 서버가 다시 시작됐다면, 터미널에 나온 새 주소로 다시 열어 주세요.",
        detail: null,
        outcome: "lock",
        retry: false,
      };
    }
    if (err.kind === "http" && err.status === 409) {
      return {
        // The item is not in the state the list showed (paused for the home
        // folder since, or a one-time job whose time has passed): reload so
        // the row shows what it is now, with its allow-home command.
        text: "지금 상태에서는 바꿀 수 없습니다. 창을 닫으면 목록을 새로 불러와 지금 상태를 보여 줍니다. 서버가 보낸 이유는 다음과 같습니다.",
        detail: err.message || null,
        outcome: "reload",
        retry: false,
      };
    }
    if (err.kind === "http" && err.status === 403) {
      return {
        text: "서버가 이 요청의 출처를 확인하지 못해 거절했습니다. 터미널에 나온 주소로 연 탭에서 다시 시도해 주세요.",
        detail: null,
        outcome: "stay",
        retry: false,
      };
    }
    if (err.kind === "http" && err.status === 404) {
      return {
        text: "이 항목을 찾지 못했습니다. 다른 곳에서 지웠을 수 있어서, 창을 닫으면 목록을 새로 불러옵니다.",
        detail: null,
        outcome: "reload",
        retry: false,
      };
    }
    if (err.kind === "http" && err.status === 503) {
      // The server gave up waiting for the jobs file lock: nothing changed.
      return {
        text: "다른 junees가 저장 파일을 쓰고 있어 지금은 바꾸지 못했습니다. 바뀐 것은 없습니다. 잠시 뒤 다시 시도해 주세요.",
        detail: null,
        outcome: "stay",
        retry: true,
      };
    }
    if (err.kind === "http") {
      // The same request gets the same 4xx; only a server fault may pass.
      return {
        text: `서버가 요청을 처리하지 못했습니다 (HTTP ${err.status}).`,
        detail: err.message || null,
        outcome: "stay",
        retry: err.status >= 500,
      };
    }
    if (err.kind === "timeout" || err.kind === "network" || err.kind === "parse") {
      // The request may have reached the server and been applied anyway
      // (a write waits for the jobs file lock), so do not call it a failure.
      return {
        text: "서버의 답을 받지 못해, 바뀌었는지 확인하지 못했습니다. 창을 닫으면 목록을 새로 불러와 지금 상태를 보여 줍니다.",
        detail: null,
        outcome: "reload",
        retry: false,
      };
    }
  }
  return { text: describeError(err).replace("불러오지 못했습니다", "바꾸지 못했습니다"), detail: null, outcome: "stay", retry: false };
}

/** Writes may wait for the jobs file lock, so they get longer than reads. */
export const WRITE_TIMEOUT_MS = 30000;

let dialogCount = 0;

/**
 * Asks before pausing or resuming `item`, then sends the request.
 * Resolves `{ outcome: "done", item }` with the changed item from the
 * server, `{ outcome: "failed", message }` when the dialog was closed
 * before a failure came back, or `{ outcome: "cancel" | "reload" | "lock" }`.
 */
export function runAction({ target, item, action, opener }) {
  const t = TARGETS[target];
  const words = ACTION_TEXT[action];
  const label = t.label(item);
  dialogCount += 1;
  const titleId = `action-title-${dialogCount}`;
  const bodyId = `action-body-${dialogCount}`;

  const error = el("div", { class: "dialog-error", role: "alert", hidden: true });
  const cancel = el("button", { type: "button", class: "button button-secondary" }, "취소");
  const confirm = el("button", { type: "button", class: "button" }, words.verb);
  const dialog = el(
    "dialog",
    { class: "dialog", "aria-labelledby": titleId, "aria-describedby": bodyId },
    el("h2", { class: "dialog-title", id: titleId }, `${t.object} ${words.ask}`),
    el(
      "div",
      { id: bodyId, class: "dialog-body" },
      el("p", { class: "dialog-target" }, label),
      el("p", null, action === "pause" ? t.pausedNote : t.resumedNote),
    ),
    error,
    el("div", { class: "dialog-actions" }, cancel, confirm),
  );

  return new Promise((resolve) => {
    let busy = false;
    let closedWhileBusy = false;
    let settled = false;
    let after = "cancel";
    let changed = null;

    const settle = (value) => {
      if (settled) return;
      settled = true;
      dialog.remove();
      if (opener) opener.disabled = false;
      resolve(value);
    };
    dialog.addEventListener("cancel", (event) => {
      // Esc while the request is out would hide its result. Chrome may
      // close anyway (no user activation since the last Esc), which the
      // close handler below allows for.
      if (busy) event.preventDefault();
    });
    dialog.addEventListener("close", () => {
      // The request still settles; report its result when it does.
      if (busy) closedWhileBusy = true;
      else settle(after === "done" ? { outcome: "done", item: changed } : { outcome: after });
    });
    cancel.addEventListener("click", () => dialog.close());
    // Leaving the screen (Back) closes an unanswered dialog, so it never
    // acts on a list that is no longer shown. A sent request still settles.
    const onLeave = () => {
      if (!busy && dialog.open) dialog.close();
    };
    window.addEventListener("hashchange", onLeave);
    dialog.addEventListener("close", () => window.removeEventListener("hashchange", onLeave), { once: true });

    confirm.addEventListener("click", async () => {
      busy = true;
      confirm.disabled = true;
      cancel.disabled = true;
      if (opener) opener.disabled = true;
      confirm.textContent = words.busy;
      error.hidden = true;
      try {
        changed = await postJson(`${t.path(item)}/${action}`, { timeoutMs: WRITE_TIMEOUT_MS });
        after = "done";
        busy = false;
        // Settle here, not only from the close event: a page in the
        // background may not get that event until it is shown again.
        if (!closedWhileBusy) dialog.close();
        settle({ outcome: "done", item: changed });
      } catch (err) {
        busy = false;
        const said = describeActionError(err);
        after = said.outcome === "stay" ? "cancel" : said.outcome;
        if (closedWhileBusy) {
          settle({ outcome: said.outcome === "stay" ? "failed" : said.outcome, message: said.text });
          return;
        }
        // replaceChildren would print a null as the text "null".
        const lines = [el("p", null, said.text)];
        if (said.detail) lines.push(el("p", { class: "dialog-detail" }, said.detail));
        error.replaceChildren(...lines);
        error.hidden = false;
        cancel.disabled = false;
        cancel.textContent = "닫기";
        if (said.retry) {
          confirm.disabled = false;
          confirm.textContent = "다시 시도";
        } else {
          confirm.hidden = true;
          cancel.focus();
        }
      }
    });

    document.body.append(dialog);
    dialog.showModal();
  });
}
