// Output tails per step, from `loops:chunks` (live, lossy) and `step_output` pages (from
// disk). Offsets are BYTE offsets into the step's log, so a missed chunk shows up as a
// gap, and the view re-reads the tail from disk instead of showing a spliced lie.

export const TAIL_CHARS = 12_000;

const encoder = typeof TextEncoder !== "undefined" ? new TextEncoder() : null;

export function byteLength(text) {
  return encoder ? encoder.encode(text).length : unescape(encodeURIComponent(text)).length;
}

function trim(text) {
  return text.length > TAIL_CHARS ? text.slice(text.length - TAIL_CHARS) : text;
}

// tails: {[stepId]: {end, text, gap}} — `end` = the byte offset just past `text`.
// Returns a new object (only the touched steps are replaced).
export function applyChunks(tails, chunks) {
  const next = { ...tails };
  for (const c of chunks || []) {
    const len = byteLength(c.text || "");
    const cur = next[c.step_id];
    if (!cur) {
      next[c.step_id] = { end: c.offset + len, text: trim(c.text || ""), gap: c.offset > 0 };
      continue;
    }
    if (c.offset + len <= cur.end) continue; // already have it
    if (c.offset === cur.end) {
      next[c.step_id] = { ...cur, end: cur.end + len, text: trim(cur.text + c.text) };
    } else if (c.offset > cur.end) {
      // Bytes between cur.end and c.offset were never seen.
      next[c.step_id] = { end: c.offset + len, text: trim(cur.text + c.text), gap: true };
    } else {
      // Overlaps what we have: keep the unseen suffix only when it starts on a character
      // boundary we can find; otherwise re-read from disk.
      next[c.step_id] = { ...cur, gap: true };
    }
  }
  return next;
}

// A page read from disk (the file's tail) replaces what we have unless live chunks have
// already moved past it.
export function applyPage(tails, stepId, page) {
  const cur = tails[stepId];
  if (cur && cur.end > page.next_offset) return tails;
  return {
    ...tails,
    [stepId]: { end: page.next_offset, text: trim(page.text || ""), gap: false, truncated: page.offset > 0 },
  };
}
