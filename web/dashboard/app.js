// Entry point: token, screens, loading and the 30-second refresh.

import { ApiError, describeError, forgetToken, getJson, getToken, takeTokenFromAddress } from "./api.js";
import { el, notice, skeleton } from "./dom.js";
import { formatClock } from "./format.js";
import * as automation from "./view-automation.js";
import * as doctor from "./view-doctor.js";
import * as sessions from "./view-sessions.js";
import * as status from "./view-status.js";
import * as usage from "./view-usage.js";

const REFRESH_MS = 30000;

const ROUTES = [
  { id: "status", title: "상태", url: () => "/api/status", render: status.render },
  {
    id: "sessions",
    title: "세션",
    url: () => `/api/sessions?limit=${sessions.LIMIT}`,
    render: sessions.render,
  },
  {
    id: "usage",
    title: "사용량",
    url: (p) => `/api/usage?days=${p.days}`,
    params: { days: usage.DEFAULT_DAYS },
    render: usage.render,
    renderControls: usage.renderControls,
  },
  { id: "cron", title: "예약 작업", url: () => "/api/cron", render: automation.renderCron },
  { id: "webhooks", title: "웹훅", url: () => "/api/webhooks", render: automation.renderWebhooks },
  { id: "doctor", title: "점검", url: () => "/api/doctor", render: doctor.render, timeoutMs: 20000 },
];

const $ = (id) => document.getElementById(id);
const ui = {
  title: $("view-title"),
  controls: $("view-controls"),
  banner: $("banner"),
  view: $("view"),
  updated: $("updated"),
  refresh: $("refresh"),
  announcer: $("announcer"),
  nav: document.querySelectorAll("[data-route]"),
};

const state = {
  route: null,
  params: {},
  savedParams: {},
  /** url -> { data, at } */
  cache: new Map(),
  error: null,
  loading: false,
  lastAttempt: 0,
  seq: 0,
  controller: null,
  locked: false,
};

function announce(text) {
  ui.announcer.textContent = "";
  // A fresh text node after clearing makes screen readers read it again.
  setTimeout(() => {
    ui.announcer.textContent = text;
  }, 50);
}

function ctx() {
  return { now: new Date(), params: state.params, setParams };
}

function setParams(next) {
  state.params = { ...state.params, ...next };
  state.savedParams[state.route.id] = state.params;
  state.error = null;
  paint();
  load();
}

function setBusy(busy) {
  state.loading = busy;
  ui.view.setAttribute("aria-busy", String(busy));
  ui.refresh.textContent = busy ? "불러오는 중" : "새로고침";
}

/** Re-renders while keeping open <details> and the focused control. */
function renderKeepingUi(render) {
  const open = new Set([...ui.view.querySelectorAll("details[id][open]")].map((d) => d.id));
  const active = document.activeElement;
  const focusId = active && ui.view.contains(active) && active.id ? active.id : null;
  render();
  for (const id of open) {
    const details = document.getElementById(id);
    if (details) details.open = true;
  }
  if (focusId) document.getElementById(focusId)?.focus({ preventScroll: true });
}

function retryButton(label = "다시 시도") {
  return el("button", { type: "button", class: "button", onclick: () => load({ manual: true }) }, label);
}

function paint() {
  if (state.locked) return;
  const route = state.route;
  const entry = state.cache.get(route.url(state.params));
  ui.banner.replaceChildren();

  if (entry) {
    try {
      renderKeepingUi(() => route.render(ui.view, entry.data, ctx()));
    } catch (err) {
      console.error(err);
      ui.view.replaceChildren(
        notice({
          title: "화면을 그리지 못했습니다",
          body: "서버 응답이 이 화면이 기대하는 형식과 다릅니다. Junees와 대시보드 파일의 버전이 맞는지 확인해 주세요.",
          tone: "fail",
        }),
      );
    }
    ui.updated.textContent = `${formatClock(entry.at)}에 갱신`;
    if (state.error) {
      ui.banner.replaceChildren(
        el(
          "div",
          { class: "callout callout-warn banner" },
          el(
            "p",
            null,
            `새 내용을 불러오지 못해 ${formatClock(entry.at)} 기준 내용을 보여 줍니다. ${describeError(state.error)}`,
          ),
          retryButton(),
        ),
      );
    }
    return;
  }

  ui.updated.textContent = "";
  if (state.error) {
    ui.view.replaceChildren(
      notice({
        title: "불러오지 못했습니다",
        body: describeError(state.error),
        tone: "fail",
        action: retryButton(),
      }),
    );
    return;
  }
  ui.view.replaceChildren(el("p", { class: "loading-text" }, "불러오는 중입니다"), skeleton(5));
}

async function load({ manual = false } = {}) {
  if (state.locked || !state.route) return;
  const route = state.route;
  const url = route.url(state.params);
  state.controller?.abort();
  const controller = new AbortController();
  state.controller = controller;
  const seq = ++state.seq;
  state.lastAttempt = Date.now();
  setBusy(true);
  try {
    const data = await getJson(url, { signal: controller.signal, timeoutMs: route.timeoutMs });
    if (seq !== state.seq) return;
    state.cache.set(url, { data, at: new Date() });
    state.error = null;
    paint();
    if (manual) announce(`${route.title} 화면을 새로 불러왔습니다.`);
  } catch (err) {
    if (seq !== state.seq) return;
    if (err instanceof ApiError && err.kind === "unauthorized") {
      lock();
      return;
    }
    if (controller.signal.aborted) return;
    state.error = err;
    paint();
    announce(`불러오지 못했습니다. ${describeError(err)}`);
  } finally {
    if (seq === state.seq) setBusy(false);
  }
}

/** The token is missing or no longer accepted: nothing else can load. */
function lock() {
  state.locked = true;
  state.controller?.abort();
  forgetToken();
  document.body.classList.add("is-locked");
  document.title = "다시 열어 주세요 - Junees 대시보드";
  ui.title.textContent = "다시 열어 주세요";
  ui.controls.replaceChildren();
  ui.banner.replaceChildren();
  ui.updated.textContent = "";
  ui.view.setAttribute("aria-busy", "false");
  ui.view.replaceChildren(
    notice({
      title: "터미널에 나온 주소로 다시 여세요",
      body: "이 페이지에는 대시보드를 열 수 있는 토큰이 없거나, 토큰이 더 이상 맞지 않습니다. junees dashboard를 실행한 터미널에 나온 주소(#token=이 붙은 주소)를 그대로 열어 주세요. 서버를 다시 시작하면 토큰도 바뀝니다.",
      tone: "warn",
    }),
  );
  announce("토큰이 맞지 않아 대시보드를 열 수 없습니다. 터미널에 나온 주소로 다시 여세요.");
}

/** A new token arrived in the address: start over with it, on the same screen. */
function acceptToken() {
  const keep = state.route?.id;
  if (keep) window.history.replaceState(null, "", `#${keep}`);
  state.locked = false;
  state.route = null;
  state.cache.clear();
  state.error = null;
  state.lastAttempt = 0;
  document.body.classList.remove("is-locked");
  route(false);
  announce("새 토큰으로 대시보드를 다시 열었습니다.");
}

function route(fromUser) {
  if (state.locked) return;
  const id = window.location.hash.replace(/^#/, "");
  const next = ROUTES.find((r) => r.id === id) ?? ROUTES[0];
  if (id && next.id !== id) window.history.replaceState(null, "", `#${next.id}`);
  if (next === state.route) return;

  state.route = next;
  state.params = { ...next.params, ...state.savedParams[next.id] };
  state.error = null;
  for (const link of ui.nav) {
    if (link.dataset.route === next.id) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  ui.title.textContent = next.title;
  document.title = `${next.title} - Junees 대시보드`;
  if (next.renderControls) next.renderControls(ui.controls, ctx());
  else ui.controls.replaceChildren();
  paint();
  load();
  if (fromUser) ui.title.focus();
}

function start() {
  takeTokenFromAddress();
  ui.refresh.addEventListener("click", () => load({ manual: true }));
  document.querySelector(".skip-link")?.addEventListener("click", (event) => {
    event.preventDefault();
    ui.title.focus();
  });
  window.addEventListener("hashchange", () => {
    if (takeTokenFromAddress()) acceptToken();
    else route(true);
  });

  if (getToken()) route(false);
  else lock();

  const refreshIfDue = () => {
    if (state.locked || document.hidden || state.loading) return;
    if (Date.now() - state.lastAttempt >= REFRESH_MS) load();
  };
  setInterval(refreshIfDue, 5000);
  document.addEventListener("visibilitychange", refreshIfDue);
}

start();
