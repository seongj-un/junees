// Entry point: token, screens, loading and the 30-second refresh.

import {
  ApiError,
  describeError,
  exchangeCode,
  forgetToken,
  getJson,
  getToken,
  takeCodeFromAddress,
  takeTokenFromAddress,
} from "./api.js";
import { el, notice, skeleton } from "./dom.js";
import { formatClock } from "./format.js";
import { parseHash } from "./routes.js";
import * as automation from "./view-automation.js";
import * as doctor from "./view-doctor.js";
import * as session from "./view-session.js";
import * as sessions from "./view-sessions.js";
import * as status from "./view-status.js";
import * as usage from "./view-usage.js";

const REFRESH_MS = 30000;

// `id` names the screen (and its nav link unless `nav` says otherwise);
// `url` gets the screen's params and the arguments from its address.
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
  {
    id: "doctor",
    title: "점검",
    url: () => "/api/doctor",
    render: doctor.render,
    timeoutMs: 20000,
    // Every call runs the checks again on the server; only on request.
    autoRefresh: false,
  },
  {
    id: "session",
    nav: "sessions",
    title: "세션",
    url: (_p, args) => `/api/sessions/${encodeURIComponent(args.id)}?offset=0&limit=${session.PAGE}`,
    render: session.render,
    renderControls: session.renderControls,
    describeError: session.describeLoadError,
    // Refreshing would only reload the first page; the button does that.
    autoRefresh: false,
  },
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
  /** Arguments from the address, such as a session id. */
  args: {},
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
  /** Write actions in progress; the 30-second refresh waits for them. */
  writes: 0,
};

function announce(text) {
  ui.announcer.textContent = "";
  // A fresh text node after clearing makes screen readers read it again.
  setTimeout(() => {
    ui.announcer.textContent = text;
  }, 50);
}

function ctx() {
  return {
    now: new Date(),
    params: state.params,
    setParams,
    announce,
    lock,
    reload: () => load({ manual: true }),
    /**
     * A write is about to start: stop the refresh until `done` is called,
     * and drop a GET already on its way, whose answer would predate the
     * write and redraw the row in its old state.
     */
    beginWrite: () => {
      state.writes += 1;
      if (state.loading) {
        state.controller?.abort();
        state.seq += 1;
        setBusy(false);
      }
      let open = true;
      return () => {
        if (!open) return;
        open = false;
        state.writes -= 1;
      };
    },
    /** Forget the cached answer for `url`, so the next visit loads it again. */
    forget: (url) => state.cache.delete(url),
    url: currentUrl(),
    setTitle: (text) => {
      ui.title.textContent = text;
      document.title = `${text} - Junees 대시보드`;
    },
    describeError,
    /** A GET from inside a screen; a refused token locks the page. */
    fetchJson: (url) => getJson(url),
    isUnauthorized: (err) => {
      if (err instanceof ApiError && err.kind === "unauthorized") {
        lock();
        return true;
      }
      return false;
    },
  };
}

function currentUrl() {
  return state.route.url(state.params, state.args);
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
  const entry = state.cache.get(currentUrl());
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
        body: route.describeError?.(state.error) ?? describeError(state.error),
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
  const url = currentUrl();
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
const LOCK_TEXT = {
  token: {
    heading: "다시 열어 주세요",
    title: "터미널에서 대시보드를 다시 열어 주세요",
    body: "이 페이지에는 대시보드를 열 수 있는 토큰이 없거나, 토큰이 더 이상 맞지 않습니다. 터미널에서 junees dashboard를 다시 실행하면 브라우저가 새로 열립니다. --no-open으로 띄웠다면 터미널에 나온 주소(#token=이 붙은 주소)를 그대로 열어 주세요. 서버를 다시 시작하면 토큰도 바뀝니다.",
    said: "토큰이 맞지 않아 대시보드를 열 수 없습니다. 터미널에서 다시 열어 주세요.",
  },
  expired: {
    heading: "주소가 만료되었습니다",
    title: "터미널에서 junees dashboard를 다시 실행하세요",
    body: "브라우저를 열 때 넘긴 주소(#code=)는 60초 안에 한 번만 쓸 수 있어서, 다시 실행해 새 주소로 열어야 합니다.",
    said: "주소가 만료되었습니다. 터미널에서 junees dashboard를 다시 실행하세요.",
  },
};

/**
 * Nothing else can load: the token is missing or refused (`token`), or the
 * one-time code from the address was used, wrong or too old (`expired`).
 * `detail` replaces the body when the reason is something else.
 */
function lock(kind = "token", detail = null) {
  const text = LOCK_TEXT[kind] ?? LOCK_TEXT.token;
  state.locked = true;
  state.controller?.abort();
  forgetToken();
  document.body.classList.add("is-locked");
  document.title = `${text.heading} - Junees 대시보드`;
  ui.title.textContent = text.heading;
  ui.controls.replaceChildren();
  ui.banner.replaceChildren();
  ui.updated.textContent = "";
  ui.view.setAttribute("aria-busy", "false");
  ui.view.replaceChildren(notice({ title: text.title, body: detail ?? text.body, tone: "warn" }));
  announce(detail ? `${text.heading}. ${detail}` : text.said);
}

/**
 * Trades a one-time `#code=` for the token, then opens the dashboard with
 * it. A refused code keeps a token this tab already has; with none, the
 * page explains that the address has expired.
 */
async function connect(code) {
  state.connecting = true;
  ui.title.textContent = "연결하는 중";
  ui.view.setAttribute("aria-busy", "true");
  ui.view.replaceChildren(el("p", { class: "loading-text" }, "대시보드에 연결하는 중입니다"), skeleton(3));
  try {
    await exchangeCode(code);
    state.connecting = false;
    acceptToken();
  } catch (err) {
    state.connecting = false;
    const refused = err instanceof ApiError && err.status === 401;
    if (getToken()) {
      // An old address opened in a tab that is already connected: stay on
      // the screen it showed.
      state.locked = false;
      state.route = null;
      if (state.hash) window.history.replaceState(null, "", state.hash);
      route(false);
      announce(refused ? "이 주소는 만료되어 쓰지 않았습니다. 지금 연결을 그대로 씁니다." : `새 주소로 연결하지 못했습니다. ${describeError(err)}`);
      return;
    }
    if (refused) lock("expired");
    else lock("token", `대시보드 서버에 연결하지 못했습니다. ${describeError(err)} 그다음 터미널에서 junees dashboard를 다시 실행하세요.`);
  }
}

/** A new token arrived in the address: start over with it, on the same screen. */
function acceptToken() {
  const keep = state.hash;
  if (keep) window.history.replaceState(null, "", keep);
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
  if (state.locked || state.connecting) return;
  const where = parseHash(window.location.hash);
  if (window.location.hash !== where.canonical) {
    window.history.replaceState(null, "", where.canonical);
  }
  const next = ROUTES.find((r) => r.id === where.name) ?? ROUTES[0];
  const sameArgs = JSON.stringify(where.args) === JSON.stringify(state.args);
  if (state.route === next && sameArgs) return;

  const previous = state.route;
  const openedSession = previous?.id === "session" ? state.args.id : null;
  state.route = next;
  state.args = where.args;
  state.hash = where.canonical;
  state.params = { ...next.params, ...state.savedParams[next.id] };
  state.error = null;
  const navId = next.nav ?? next.id;
  for (const link of ui.nav) {
    // On a session's own screen the 세션 link is the section, not the page.
    if (link.dataset.route === navId) link.setAttribute("aria-current", next.nav ? "true" : "page");
    else link.removeAttribute("aria-current");
  }
  ui.title.textContent = next.title;
  document.title = `${next.title} - Junees 대시보드`;
  if (next.renderControls) next.renderControls(ui.controls, ctx());
  else ui.controls.replaceChildren();
  paint();
  load();
  if (!fromUser) return;
  // Back from a session: return to its row, not the top of the page.
  const row = openedSession && next.id === "sessions" ? ui.view.querySelector(`[data-session-link="${CSS.escape(openedSession)}"]`) : null;
  if (row) row.focus();
  else ui.title.focus();
}

function start() {
  const code = takeCodeFromAddress();
  takeTokenFromAddress();
  ui.refresh.addEventListener("click", () => load({ manual: true }));
  document.querySelector(".skip-link")?.addEventListener("click", (event) => {
    event.preventDefault();
    ui.title.focus();
  });
  window.addEventListener("hashchange", () => {
    const pasted = takeCodeFromAddress();
    if (pasted) connect(pasted);
    else if (takeTokenFromAddress()) acceptToken();
    else route(true);
  });

  if (code) connect(code);
  else if (getToken()) route(false);
  else lock();

  const refreshIfDue = () => {
    if (state.locked || state.connecting || document.hidden || state.loading || state.writes > 0) return;
    if (state.route?.autoRefresh === false) return;
    if (Date.now() - state.lastAttempt >= REFRESH_MS) load();
  };
  setInterval(refreshIfDue, 5000);
  document.addEventListener("visibilitychange", refreshIfDue);
}

start();
