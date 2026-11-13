// The undo parts of the 대화 screen (stage 20): the files a turn changed,
// the full change list, the confirmation dialog and the result.

import { postJson } from "./api.js";
import { changeLabel, describeUndoError, relPath, undoOutcome, undoPlan } from "./checkpoints.js";

/**
 * What a checkpoint offers: "되돌리기" (or "이 되돌리기 취소" for an undo),
 * a note that a later undo put it back, or that nothing in it can come
 * back. `actions` is `{ onUndo, list, by }`, or null where undo is off.
 */
function undoAction(item, actions, label) {
  if (!actions) return null;
  const undone = actions.by.get(item.turn);
  if (undone !== undefined) return el("p", { class: "cp-state" }, `되돌리기(변경 기록 ${undone})에 포함됐습니다.`);
  if (undoPlan(actions.list, item.turn).back === 0) return el("p", { class: "cp-state" }, "되돌릴 수 있는 파일이 없습니다.");
  return undoButton(item, item.undid.length ? "이 되돌리기 취소" : label, actions.onUndo);
}
import { badge, el, pathCode } from "./dom.js";
import { formatDateTime, parseTime } from "./format.js";

/** Undo waits for the files to be written; like the other writes, longer than a read. */
const UNDO_TIMEOUT_MS = 30000;

let dialogs = 0;

/** One line per file: how the turn changed it, its path, and whether an undo would leave it. */
export function fileList(files, cwd, { plan = false } = {}) {
  return el(
    "ul",
    { class: "cp-list" },
    files.map((file) =>
      el(
        "li",
        { class: "cp-file" },
        el("span", { class: `cp-change cp-change-${file.change}` }, changeLabel(file.change)),
        pathCode(relPath(file.path, cwd)),
        file.restorable ? null : badge(plan ? "그대로 둠" : "되돌릴 수 없음", "warn"),
      ),
    ),
  );
}

/** "11월 13일 오후 3:12", or nothing without a time. */
function when(item) {
  const at = parseTime(item.createdAt);
  return at ? formatDateTime(at, new Date()) : null;
}

function undoButton(item, label, onUndo) {
  return el(
    "button",
    { type: "button", class: "button button-secondary button-small cp-undo", "data-undo": String(item.turn), onclick: (event) => onUndo(item.turn, event.currentTarget) },
    label,
  );
}

/**
 * The end of a turn that changed files: "파일 N개 바뀜" (open it for the
 * paths) and "이 턴부터 되돌리기" (see undoAction for `actions`).
 */
export function turnFooter(item, cwd, actions) {
  const cannot = item.files.filter((file) => !file.restorable).length;
  const parts = [
    el(
      "details",
      { class: "cp-files", id: `cp-files-${item.turn}` },
      el(
        "summary",
        null,
        el("span", { class: "cp-sum" }, item.undid.length ? `되돌리기로 파일 ${item.files.length}개 바뀜` : `파일 ${item.files.length}개 바뀜`),
        cannot ? badge(`${cannot}개 되돌릴 수 없음`, "warn") : null,
        el("span", { class: "cp-num" }, `변경 기록 ${item.turn}`),
      ),
      fileList(item.files, cwd),
      cannot
        ? el("p", { class: "cp-help" }, "되돌릴 수 없는 파일은 그 뒤에 사람이나 bash가 바꿨거나, 너무 커서 내용을 기록하지 않은 파일입니다.")
        : null,
    ),
  ];
  parts.push(undoAction(item, actions, "이 턴부터 되돌리기"));
  return el("div", { class: "cp", "data-checkpoint": String(item.turn) }, parts);
}

/**
 * Every checkpoint of the chat, newest first, folded above the chat: the
 * ones older than the shown history, and undos, have no turn to sit on.
 */
export function changePanel(list, cwd, actions, open) {
  const items = [...list].reverse();
  return el(
    "details",
    { class: "cp-panel card", id: "cp-panel", open: open || null },
    el("summary", null, "이 대화의 파일 변경 기록", el("span", { class: "cp-count" }, `${items.length}개`)),
    el(
      "p",
      { class: "cp-help" },
      "Junees가 write_file, edit_file로 바꾼 파일을 턴마다 기록해 둡니다. bash로 바꾼 파일은 기록하지 않습니다. 한 기록부터 되돌리면 그 뒤의 기록도 최근 것부터 함께 되돌립니다.",
    ),
    el(
      "ol",
      { class: "cp-entries", "aria-label": "파일 변경 기록, 최근 것부터" },
      items.map((item) => {
        const time = when(item);
        return el(
          "li",
          { class: "cp-entry" },
          el(
            "p",
            { class: "cp-entry-head" },
            el("span", { class: "cp-entry-title" }, item.undid.length ? `변경 기록 ${item.turn} (되돌리기)` : `변경 기록 ${item.turn}`),
            time ? el("span", { class: "cp-entry-time" }, time) : null,
          ),
          fileList(item.files, cwd),
          undoAction(item, actions, "여기부터 되돌리기"),
        );
      }),
    ),
  );
}

/**
 * Asks before undoing from checkpoint `from`, then sends it. Resolves
 * `{ outcome: "done", answer }` with the server's answer,
 * `{ outcome: "failed", message }` when the dialog was closed before a
 * failure came back, or `{ outcome: "cancel" | "reload" | "off" }`.
 * A 401 is passed on as `{ outcome: "lock", error }`.
 */
export function confirmUndo({ chatId, list, from, cwd, opener, isUnauthorized }) {
  const plan = undoPlan(list, from);
  dialogs += 1;
  const titleId = `undo-title-${dialogs}`;
  const bodyId = `undo-body-${dialogs}`;
  const many = plan.turns.length > 1;
  // No particle after a number (과/와, 을/를 depend on how it is read).
  const scope = many
    ? `변경 기록 ${from}부터 ${plan.turns[0].turn}까지 ${plan.turns.length}개를 최근 것부터 되돌립니다. 그 뒤에 파일을 바꾼 턴도 함께 되돌아갑니다.`
    : "아래 파일을 이 턴 전의 내용으로 되돌립니다.";
  const error = el("div", { class: "dialog-error", role: "alert", hidden: true });
  // Cancel takes the first focus, not the scrolling list before it.
  const cancel = el("button", { type: "button", class: "button button-secondary", autofocus: true }, "취소");
  const confirm = el("button", { type: "button", class: "button" }, "되돌리기");
  const notes = [el("p", null, scope)];
  for (const item of plan.turns) {
    const time = when(item);
    notes.push(
      el("p", { class: "cp-plan-head" }, item.undid.length ? `변경 기록 ${item.turn} (되돌리기)` : `변경 기록 ${item.turn}`, time ? ` · ${time}` : ""),
      fileList(item.files, cwd, { plan: true }),
    );
  }
  if (plan.kept) {
    notes.push(
      el(
        "p",
        { class: "cp-plan-warn" },
        `${plan.kept}개는 그대로 둡니다. 그 뒤에 사람이나 bash가 바꿨거나 너무 커서 기록하지 않은 파일이라, 덮어쓰지 않습니다.`,
      ),
    );
  }
  notes.push(
    el(
      "p",
      { class: "cp-help" },
      "bash로 바꾼 파일은 기록하지 않아 되돌리지 않습니다. 되돌린 내용은 다음 메시지와 함께 Junees에게 알리고, 되돌리기도 기록되므로 다시 되돌릴 수 있습니다.",
    ),
  );
  if (plan.back === 0) {
    confirm.disabled = true;
    notes.push(el("p", { class: "cp-plan-warn" }, "되돌릴 수 있는 파일이 없어 되돌리기를 할 수 없습니다."));
  }
  const dialog = el(
    "dialog",
    { class: "dialog dialog-wide", "aria-labelledby": titleId, "aria-describedby": bodyId },
    el("h2", { class: "dialog-title", id: titleId }, many ? `변경 기록 ${from}부터 되돌릴까요?` : "이 턴의 파일 변경을 되돌릴까요?"),
    el("div", { id: bodyId, class: "dialog-body dialog-scroll", tabindex: "0" }, notes),
    error,
    el("div", { class: "dialog-actions" }, cancel, confirm),
  );

  return new Promise((resolve) => {
    let busy = false;
    let closedWhileBusy = false;
    let settled = false;
    let after = "cancel";
    let answer = null;
    const settle = (value) => {
      if (settled) return;
      settled = true;
      dialog.remove();
      if (opener?.isConnected) opener.disabled = false;
      resolve(value);
    };
    dialog.addEventListener("cancel", (event) => {
      if (busy) event.preventDefault();
    });
    dialog.addEventListener("close", () => {
      if (busy) closedWhileBusy = true;
      else settle(after === "done" ? { outcome: "done", answer } : { outcome: after });
    });
    cancel.addEventListener("click", () => dialog.close());
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
      confirm.textContent = "되돌리는 중";
      error.hidden = true;
      try {
        answer = await postJson(`/api/chat/${encodeURIComponent(chatId)}/undo`, { payload: { turn: from }, timeoutMs: UNDO_TIMEOUT_MS });
        after = "done";
        busy = false;
        if (!closedWhileBusy) dialog.close();
        settle({ outcome: "done", answer });
      } catch (err) {
        busy = false;
        if (isUnauthorized(err)) {
          if (dialog.open) dialog.close();
          settle({ outcome: "lock" });
          return;
        }
        const said = describeUndoError(err);
        after = said.after === "stay" ? "cancel" : said.after;
        if (closedWhileBusy) {
          settle({ outcome: said.after === "stay" ? "failed" : said.after, message: said.text });
          return;
        }
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

/** The result of an undo, as an item of the chat. */
export function outcomeView(answer, cwd) {
  const outcome = undoOutcome(answer);
  const parts = [
    el("p", { class: "msg-head" }, el("span", { class: "msg-role" }, "파일 되돌림")),
    el("p", { class: "cp-headline" }, outcome.headline),
  ];
  if (outcome.restored.length) {
    parts.push(
      el("p", { class: "cp-label" }, "되돌린 파일"),
      el(
        "ul",
        { class: "cp-list" },
        outcome.restored.map((file) =>
          el(
            "li",
            { class: "cp-file" },
            pathCode(relPath(file.path, cwd)),
            file.partial ? [badge("일부만 되돌림", "warn"), el("p", { class: "cp-partial" }, file.partial)] : null,
          ),
        ),
      ),
    );
  }
  if (outcome.skipped.length) {
    parts.push(
      el("p", { class: "cp-label" }, "그대로 둔 파일"),
      el(
        "ul",
        { class: "cp-list" },
        outcome.skipped.map((file) => el("li", { class: "cp-file" }, pathCode(relPath(file.path, cwd)), el("span", { class: "cp-reason" }, file.reason))),
      ),
    );
  }
  parts.push(el("p", { class: "msg-note" }, "다음 메시지를 보낼 때 이 결과를 Junees에게 함께 알립니다."));
  // The undo's own checkpoint goes here once the list has it ("이 되돌리기 취소").
  const slot = el("div", { class: "cp-slot" });
  parts.push(slot);
  return { node: el("li", { class: "msg cp-result", tabindex: "-1" }, parts), headline: outcome.headline, slot, changed: outcome.restored.length > 0 };
}
