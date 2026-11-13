"""File checkpoints for the dev chat (`serve.py --chat`), as stage 20 has
them: the files a scripted turn writes live in memory, every turn that
changes one leaves a checkpoint, and GET .../checkpoints and POST .../undo
answer as src/dashboard/chat/mod.rs does (numbers count only the turns
that changed files, `restorable` is worked out for undoing from that
checkpoint, an undo is recorded too (`undid` in the list, `checkpoint` in
its answer, as #118) and leaves a [되돌림] line for the
next message). GET /__dev/edit?path=<file> changes a file the way bash
would, without a checkpoint.
"""

import time

CWD = "/home/me/work/junees"

# src/checkpoints/mod.rs `reason`
CHANGED = "바뀐 뒤 수정됨"
KEPT_CREATED = "턴이 만든 파일이 그 뒤 수정되어 지우지 않음"


def now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def absolute(path):
    return path if path.startswith("/") else f"{CWD}/{path}"


class Checkpoints:
    """The files of one dev server and the checkpoints of each session."""

    def __init__(self, on=True):
        self.on = on
        self.files = {}  # absolute path -> content
        self.sessions = {}  # session id -> [manifest]
        self.notes = {}  # session id -> [line]

    def write(self, session_id, turn_files):
        """Writes `{path: content or None}` for a model turn; returns its checkpoint number."""
        changed = []
        for path, content in turn_files.items():
            path = absolute(path)
            before = self.files.get(path)
            if content is None:
                self.files.pop(path, None)
            else:
                self.files[path] = content
            changed.append({"path": path, "before": before, "after": content})
        if not self.on or not changed:
            return None
        return self.record(session_id, changed, [])

    def record(self, session_id, changed, undid):
        turns = self.sessions.setdefault(session_id, [])
        turn = len(turns) + 1
        turns.append({"turn": turn, "created_at": now(), "undid": undid, "files": changed})
        return turn

    def edit(self, path):
        """A change from outside (a person or bash): no checkpoint."""
        path = absolute(path)
        self.files[path] = self.files.get(path, "") + "\n(밖에서 고침)"
        return path

    def _walk(self, session_id, start, apply):
        """Undoes from `start` on a copy (or for real): `[(path, turn, reason or None)]`, newest first."""
        state = self.files if apply else dict(self.files)
        steps = []
        for manifest in sorted(self.sessions.get(session_id, []), key=lambda m: -m["turn"]):
            if manifest["turn"] < start:
                break
            for file in reversed(manifest["files"]):
                path = file["path"]
                if state.get(path) != file["after"]:
                    steps.append((path, manifest["turn"], KEPT_CREATED if file["before"] is None else CHANGED))
                    continue
                if file["before"] is None:
                    state.pop(path, None)
                else:
                    state[path] = file["before"]
                steps.append((path, manifest["turn"], None))
        return steps

    def listing(self, session_id):
        """GET /api/chat/<id>/checkpoints."""
        if not self.on:
            return []
        out = []
        for manifest in self.sessions.get(session_id, []):
            steps = self._walk(session_id, manifest["turn"], apply=False)
            ok = {path for path, turn, reason in steps if turn == manifest["turn"] and reason is None}
            out.append({
                "turn": manifest["turn"],
                "created_at": manifest["created_at"],
                "undid": manifest["undid"],
                "files": [
                    {
                        "path": f["path"],
                        "change": "created" if f["before"] is None else "deleted" if f["after"] is None else "modified",
                        "restorable": f["path"] in ok,
                    }
                    for f in manifest["files"]
                ],
            })
        return out

    def undo(self, session_id, start):
        """(status, body) for POST /api/chat/<id>/undo."""
        if not self.on:
            return 409, {"error": "checkpoints_off", "message": "file checkpoints are off (checkpoints = false)"}
        turns = [m["turn"] for m in self.sessions.get(session_id, []) if m["turn"] >= start]
        if not turns:
            return 404, {"error": "not_found", "message": f"there is no checkpoint at turn {start} or later"}
        before = dict(self.files)
        steps = self._walk(session_id, start, apply=True)
        restored, skipped, partial, seen = [], [], [], []
        for path in [p for p, _, _ in steps]:
            if path not in seen:
                seen.append(path)
        for path in seen:
            mine = [(turn, reason) for p, turn, reason in steps if p == path]
            done = [turn for turn, reason in mine if reason is None]
            miss = next(((turn, reason) for turn, reason in mine if reason is not None), None)
            if miss is None:
                restored.append(path)
            elif done:
                restored.append(path)
                partial.append({"path": path, "until_turn": min(done), "skipped_turn": miss[0], "reason": miss[1]})
            else:
                skipped.append({"path": path, "reason": miss[1]})
        made = None
        if restored:
            changed = [{"path": p, "before": before.get(p), "after": self.files.get(p)} for p in restored]
            made = self.record(session_id, changed, sorted(turns, reverse=True))
        listed = ", ".join(str(t) for t in sorted(turns))
        if restored:
            names = ", ".join(
                next((f"{p} (변경 기록 {x['until_turn']}까지 되돌림, 변경 기록 {x['skipped_turn']}은 건너뜀: {x['reason']})" for x in partial if x["path"] == p), p)
                for p in restored
            )
            note = f"[되돌림] 변경 기록 {listed}의 파일 변경을 되돌렸습니다: {names}"
        else:
            note = f"[되돌림] 변경 기록 {listed}의 파일 변경을 되돌리지 못했습니다"
        if skipped:
            note += " (건너뜀: " + "; ".join(f"{s['path']}, {s['reason']}" for s in skipped) + ")"
        note += ". bash로 바꾼 파일은 기록되지 않아 되돌리지 않았습니다."
        self.notes.setdefault(session_id, []).append(note)
        body = {"restored": restored, "skipped": skipped, "partial": partial}
        if made is not None:
            body["checkpoint"] = made
        return 200, body

    def take_notes(self, session_id):
        return self.notes.pop(session_id, [])
