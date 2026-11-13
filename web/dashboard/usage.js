// Pure helpers for the usage screen: one value per day and per model.
// No DOM here, so `node --test` can load them.

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

/**
 * One entry per day for the last `count` days, days without use as zero.
 * The range ends today, or at the newest day in `rows` when the server's
 * date is ahead of this browser's.
 */
export function dailyTotals(rows, count, today) {
  const newest = rows.reduce((max, row) => (row.date > max ? row.date : max), today);
  const days = dayRange(newest, count);
  const byDay = new Map(days.map((day) => [day, { date: day, cost: 0, input: 0, output: 0 }]));
  for (const row of rows) {
    const entry = byDay.get(row.date);
    if (!entry) continue;
    entry.cost += num(row.cost_usd);
    entry.input += num(row.input_tokens);
    entry.output += num(row.output_tokens);
  }
  return days.map((day) => byDay.get(day));
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
    };
    entry.input += num(row.input_tokens);
    entry.output += num(row.output_tokens);
    entry.cacheRead += num(row.cache_read_tokens);
    entry.cacheWrite += num(row.cache_write_tokens);
    entry.cost += num(row.cost_usd);
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
