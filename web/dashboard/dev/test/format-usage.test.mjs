// Run with: node --test web/dashboard/dev/test/
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  formatCompact,
  formatDay,
  formatDayShort,
  formatDuration,
  formatInt,
  formatRelative,
  formatUsd,
  parseTime,
} from "../../format.js";
import { dailyTotals, dayRange, modelTotals, niceCeil } from "../../usage.js";

test("formatUsd keeps tiny costs visible and handles missing values", () => {
  assert.equal(formatUsd(0), "$0.00");
  assert.equal(formatUsd(0.004), "$0.01 미만");
  assert.equal(formatUsd(0.012), "$0.01");
  assert.equal(formatUsd(1234.5), "$1,234.50");
  assert.equal(formatUsd(null), "-");
  assert.equal(formatUsd(Number.NaN), "-");
});

test("formatInt and formatCompact", () => {
  assert.equal(formatInt(1234567), "1,234,567");
  assert.equal(formatInt(undefined), "-");
  assert.equal(formatCompact(9999), "9,999");
  assert.equal(formatCompact(5821000), "582.1만");
});

test("parseTime rejects broken input", () => {
  assert.equal(parseTime(null), null);
  assert.equal(parseTime(""), null);
  assert.equal(parseTime("not a time"), null);
  assert.equal(parseTime("2026-11-13T01:02:03Z").toISOString(), "2026-11-13T01:02:03.000Z");
});

test("formatRelative covers past, future and now", () => {
  const now = new Date("2026-11-13T12:00:00Z");
  assert.equal(formatRelative(new Date("2026-11-13T11:59:40Z"), now), "방금");
  assert.equal(formatRelative(new Date("2026-11-13T11:57:00Z"), now), "3분 전");
  assert.equal(formatRelative(new Date("2026-11-13T14:00:00Z"), now), "2시간 후");
  assert.equal(formatRelative(new Date("2026-11-10T12:00:00Z"), now), "3일 전");
  assert.equal(formatRelative(null, now), "-");
});

test("formatDuration", () => {
  assert.equal(formatDuration(45), "45초");
  assert.equal(formatDuration(754), "12분");
  assert.equal(formatDuration(3600), "1시간");
  assert.equal(formatDuration(3 * 3600 + 5 * 60), "3시간 5분");
  assert.equal(formatDuration(2 * 86400 + 4 * 3600), "2일 4시간");
  assert.equal(formatDuration(-1), "-");
  assert.equal(formatDuration(null), "-");
});

test("formatDay and formatDayShort", () => {
  assert.equal(formatDay("2026-11-03"), "11월 3일");
  assert.equal(formatDayShort("2026-11-03"), "11.3");
  assert.equal(formatDay("bad"), "bad");
});

test("dayRange crosses month ends, oldest first", () => {
  assert.deepEqual(dayRange("2026-03-02", 4), ["2026-02-27", "2026-02-28", "2026-03-01", "2026-03-02"]);
  assert.equal(dayRange("2026-11-13", 90).length, 90);
});

test("dailyTotals fills missing days and sums models of the same day", () => {
  const rows = [
    { date: "2026-11-12", model: "a", input_tokens: 10, output_tokens: 1, cost_usd: 0.5 },
    { date: "2026-11-12", model: "b", input_tokens: 5, output_tokens: 2, cost_usd: 0.25 },
    { date: "2026-11-10", model: "a", input_tokens: 1, output_tokens: 1, cost_usd: null },
  ];
  const days = dailyTotals(rows, 4, "2026-11-13");
  assert.deepEqual(
    days.map((d) => [d.date, d.cost, d.input, d.output]),
    [
      ["2026-11-10", 0, 1, 1],
      ["2026-11-11", 0, 0, 0],
      ["2026-11-12", 0.75, 15, 3],
      ["2026-11-13", 0, 0, 0],
    ],
  );
});

test("dailyTotals ends at the server's newest day when it is ahead of the browser", () => {
  const rows = [{ date: "2026-11-14", model: "a", input_tokens: 1, output_tokens: 1, cost_usd: 1 }];
  const days = dailyTotals(rows, 2, "2026-11-13");
  assert.deepEqual(
    days.map((d) => d.date),
    ["2026-11-13", "2026-11-14"],
  );
  assert.equal(days[1].cost, 1);
});

test("modelTotals sorts by cost and names a missing model", () => {
  const rows = [
    { date: "d", model: "cheap", input_tokens: 100, output_tokens: 0, cost_usd: 0.1 },
    { date: "d", model: "dear", input_tokens: 1, output_tokens: 0, cache_read_tokens: 7, cost_usd: 2 },
    { date: "e", model: "cheap", input_tokens: 100, output_tokens: 0, cost_usd: 0.1 },
    { date: "e", model: null, input_tokens: 1, output_tokens: 0, cost_usd: 0 },
  ];
  const totals = modelTotals(rows);
  assert.deepEqual(
    totals.map((m) => m.model),
    ["dear", "cheap", "알 수 없는 모델"],
  );
  assert.equal(totals[1].input, 200);
  assert.equal(totals[0].cacheRead, 7);
});

test("niceCeil rounds up to 1, 2, 2.5 or 5 steps", () => {
  assert.equal(niceCeil(0), 0);
  assert.equal(niceCeil(2.34), 2.5);
  assert.equal(niceCeil(0.004), 0.005);
  assert.equal(niceCeil(7), 10);
  assert.equal(niceCeil(100), 100);
  assert.equal(niceCeil(101), 200);
});
