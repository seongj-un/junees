// Pure formatting helpers. No DOM here, so `node --test` can load them.

const intFormat = new Intl.NumberFormat("ko-KR");

/** `1234` -> `1,234`; `null` -> `-`. */
export function formatInt(value) {
  if (typeof value !== "number" || !Number.isFinite(value)) return "-";
  return intFormat.format(Math.round(value));
}

/** `12345678` -> `1,235만`; under 10,000 it stays a plain number. */
export function formatCompact(value) {
  if (typeof value !== "number" || !Number.isFinite(value)) return "-";
  if (Math.abs(value) < 10000) return formatInt(value);
  return new Intl.NumberFormat("ko-KR", { notation: "compact", maximumFractionDigits: 1 }).format(
    value,
  );
}

/** Dollars with cents; a tiny positive cost does not round to `$0.00`. */
export function formatUsd(value) {
  if (typeof value !== "number" || !Number.isFinite(value)) return "-";
  if (value > 0 && value < 0.01) return "$0.01 미만";
  return `$${value.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

/** An RFC 3339 string as a `Date`, or `null` when missing or broken. */
export function parseTime(value) {
  if (typeof value !== "string" || value === "") return null;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? null : date;
}

/** `11월 13일 오후 3:04`, with the year when it is not this year. */
export function formatDateTime(date, now = new Date()) {
  if (!date) return "-";
  const options = { month: "long", day: "numeric", hour: "numeric", minute: "2-digit" };
  if (date.getFullYear() !== now.getFullYear()) options.year = "numeric";
  return new Intl.DateTimeFormat("ko-KR", options).format(date);
}

/** `오후 3:04:05`. */
export function formatClock(date) {
  return new Intl.DateTimeFormat("ko-KR", {
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
  }).format(date);
}

const relativeFormat = new Intl.RelativeTimeFormat("ko-KR", { numeric: "auto" });

/** `3분 전`, `2시간 후`, `방금`. */
export function formatRelative(date, now = new Date()) {
  if (!date) return "-";
  const seconds = Math.round((date.getTime() - now.getTime()) / 1000);
  const abs = Math.abs(seconds);
  if (abs < 45) return seconds <= 0 ? "방금" : "곧";
  const minutes = Math.round(seconds / 60);
  if (Math.abs(minutes) < 60) return relativeFormat.format(minutes, "minute");
  const hours = Math.round(seconds / 3600);
  if (Math.abs(hours) < 24) return relativeFormat.format(hours, "hour");
  const days = Math.round(seconds / 86400);
  if (Math.abs(days) < 30) return relativeFormat.format(days, "day");
  const months = Math.round(days / 30);
  if (Math.abs(months) < 12) return relativeFormat.format(months, "month");
  return relativeFormat.format(Math.round(days / 365), "year");
}

/** `45초`, `12분`, `3시간 5분`, `2일 4시간`. */
export function formatDuration(totalSeconds) {
  if (typeof totalSeconds !== "number" || !Number.isFinite(totalSeconds) || totalSeconds < 0) {
    return "-";
  }
  const s = Math.floor(totalSeconds);
  if (s < 60) return `${s}초`;
  const minutes = Math.floor(s / 60);
  if (minutes < 60) return `${minutes}분`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return minutes % 60 ? `${hours}시간 ${minutes % 60}분` : `${hours}시간`;
  const days = Math.floor(hours / 24);
  return hours % 24 ? `${days}일 ${hours % 24}시간` : `${days}일`;
}

/** `2026-11-13` -> `11월 13일`. */
export function formatDay(day) {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(day ?? "");
  if (!match) return day ?? "-";
  return `${Number(match[2])}월 ${Number(match[3])}일`;
}

/** `2026-11-13` -> `11.13`, for chart axes. */
export function formatDayShort(day) {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(day ?? "");
  if (!match) return day ?? "";
  return `${Number(match[2])}.${Number(match[3])}`;
}
