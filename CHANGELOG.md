# Changelog

## 1.2.0 — 2026-11-13

- Deletes are instant now: after a delete, the app filters its cached lists in place instead of rescanning the whole index (with a 2M-file index that rebuild — including cloning and sorting all entries just to pick the 200 largest — was what made even a 4-byte delete feel slow). Background pruning of the saved index is also coalesced instead of rewriting the file after every single delete.
- "Restart as administrator" button in the sidebar (shown when the app is not elevated): deleting files inside C:\Windows, Program Files and other protected locations requires an elevated token — the app now says so explicitly when Windows denies access, instead of leaving the failure cryptic.
- Rendering hardening: window/popup shadows off, and Light UI turns on automatically for RDP sessions when no theme choice has been saved.

## 1.1.0 — 2026-11-13

- Multi-select delete: checkbox column (with select-all) in the Search and Largest-files tables and in Cleanup-tips detail lists, plus a selection bar with bulk Recycle Bin / permanent delete.
- Permanent-delete confirmations now offer "Don't ask again" (remembered across runs); a sidebar toggle "Ask before permanent delete" brings the confirmation back at any time. When remembered, every permanent-delete button acts immediately.
- Fixed two stacked path tooltips when hovering a file row (the file-name cell carried the same tooltip as the path cell; tooltip fade stacking removed as well).
- Fixed doubled checkmarks ("✔✔") on permanent-delete confirm buttons.
- Flicker reduction, round two: the glass background (gradient + glow spots) is now rendered once into a static texture instead of being re-drawn every frame, and widget hover transitions are instant (animation time 0), so repaints are pixel-identical — much friendlier to remote-desktop encoders.

## 1.0.0 — 2026-11-13

First public release under the FileSweep name.

- English UI (the app was developed as "TimFile" v1–v23 with a Vietnamese UI).
- Light UI mode (flat, no translucency) for weak GPUs and remote-desktop sessions; hover-static in light mode to minimize repaints.
- Fixed tables not stretching to the full window width when maximized (remainder column).
- Fixed single-file deletes from a Cleanup-tips card not removing the row from that card's detail list.
- Files already gone from disk are treated as handled (removed from the lists) instead of erroring with "os error 2"; small deletes now update all lists immediately.
- Persistent scan index and duplicate results per root, reloaded at startup; hash cache for near-instant duplicate re-runs; hardlink detection.
