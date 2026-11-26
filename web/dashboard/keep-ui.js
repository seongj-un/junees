// Redrawing a screen on auto-refresh without losing what the person set up.

/**
 * Attribute that marks a scroll area whose position survives a redraw. Its
 * value names the area, and must change when the content is a different
 * view of the data (the usage table includes its period), so that a new
 * period starts at the top again.
 */
export const SCROLL_KEY = "data-scroll-key";

/**
 * Runs `render()`, which replaces what is inside `view`, then puts back the
 * open <details>, the scroll position of every keyed scroll area and the
 * focused control (all matched by id or key).
 */
export function renderKeepingUi(view, render) {
  const open = new Set([...view.querySelectorAll("details[id][open]")].map((d) => d.id));
  const scrolls = new Map(
    [...view.querySelectorAll(`[${SCROLL_KEY}]`)].map((node) => [
      node.getAttribute(SCROLL_KEY),
      { top: node.scrollTop, left: node.scrollLeft },
    ]),
  );
  const active = document.activeElement;
  const focusId = active && view.contains(active) && active.id ? active.id : null;
  render();
  // Open first: a closed <details> has no layout, so its content cannot scroll.
  for (const id of open) {
    const details = document.getElementById(id);
    if (details) details.open = true;
  }
  for (const node of view.querySelectorAll(`[${SCROLL_KEY}]`)) {
    const was = scrolls.get(node.getAttribute(SCROLL_KEY));
    if (!was) continue;
    node.scrollTop = was.top;
    node.scrollLeft = was.left;
  }
  if (focusId) document.getElementById(focusId)?.focus({ preventScroll: true });
}
