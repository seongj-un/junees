// Pure helpers for the usage screen: one value per day and per model.
// No DOM here, so `node --test` can load them.

import { formatUsd } from "./format.js";

/** `YYYY-MM-DD` of `date` in local time. */
export function localDay(date) {
  const y = date.getFullYear();
  const m = String(date.getMonth() + 1).padStart(2, "0");
  const d = String(date.getDate()).padStart(2, "0");
  return `${y}-${m}-${d}`;
}

/** The `count` days ending at `end` (`YYYY-MM-DD`), oldest first. */
export function dayRange(end, count) {
  const [y, m, d] = end.split("-").map(Number);
  const days = [];
  for (let i = count - 1; i >= 0; i -= 1) {
    days.push(localDay(new Date(y, m - 1, d - i)));
  }
  return days;
}

const num = (value) => (typeof value === "number" && Number.isFinite(value) ? value : 0);
const priced = (row) => typeof row.cost_usd === "number" && Number.isFinite(row.cost_usd);

/** The day `count - 1` days after `day` (`YYYY-MM-DD`). */
function lastDayFrom(day, count) {
  const [y, m, d] = day.split("-").map(Number);
  return localDay(new Date(y, m - 1, d + count - 1));
}

/**
 * Where the `count`-day range ends. The server splits days in its own time
 * zone, which can be a day off this browser's: the range ends today, but
 * never before the newest day sent and never so late that the oldest day
 * sent falls out of it.
 */
export function rangeEnd(rows, count, today) {
  if (rows.length === 0) return today;
  const dates = rows.map((row) => row.date).sort();
  const oldest = dates[0];
  const newest = dates[dates.length - 1];
  const latest = lastDayFrom(oldest, count);
  const end = today > latest ? latest : today;
  return end < newest ? newest : end;
}

/**
 * One entry per day for the `count` days ending at {@link rangeEnd}, days
 * without use as zero. `unpriced` marks a day with use the server could not
 * price (`cost_usd: null`); `cost` then only sums the priced part.
 */
export function dailyTotals(rows, count, today) {
  const days = dayRange(rangeEnd(rows, count, today), count);
  const byDay = new Map(
    days.map((day) => [day, { date: day, cost: 0, input: 0, output: 0, unpriced: false }]),
  );
  for (const row of rows) {
    const entry = byDay.get(row.date);
    if (!entry) continue;
    entry.cost += num(row.cost_usd);
    entry.unpriced ||= !priced(row);
    entry.input += num(row.input_tokens);
    entry.output += num(row.output_tokens);
  }
  return days.map((day) => byDay.get(day));
}

/** Whether any row came without a price. */
export function hasUnpriced(rows) {
  return rows.some((row) => !priced(row));
}

/** A cost that may be missing its price, as text. */
export function costLabel(cost, unpriced) {
  if (!unpriced) return formatUsd(cost);
  if (!(cost > 0)) return "가격 없음";
  return `${formatUsd(cost)} (일부 가격 없음)`;
}

/** Totals per model, the most expensive first. */
export function modelTotals(rows) {
  const byModel = new Map();
  for (const row of rows) {
    const name = row.model ?? "알 수 없는 모델";
    const entry = byModel.get(name) ?? {
      model: name,
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
      cost: 0,
      unpriced: false,
    };
    entry.input += num(row.input_tokens);
    entry.output += num(row.output_tokens);
    entry.cacheRead += num(row.cache_read_tokens);
    entry.cacheWrite += num(row.cache_write_tokens);
    entry.cost += num(row.cost_usd);
    entry.unpriced ||= !priced(row);
    byModel.set(name, entry);
  }
  return [...byModel.values()].sort(
    (a, b) => b.cost - a.cost || b.input + b.output - (a.input + a.output),
  );
}

/** A round axis maximum at or above `value` (1, 2, 2.5, 5 x 10^n). */
export function niceCeil(value) {
  if (!(value > 0)) return 0;
  const power = 10 ** Math.floor(Math.log10(value));
  for (const step of [1, 2, 2.5, 5, 10]) {
    if (step * power >= value - 1e-12) return step * power;
  }
  return 10 * power;
}
