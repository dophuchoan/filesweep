# Changelog

## 1.0.0 — 2026-11-13

First public release under the FileSweep name.

- English UI (the app was developed as "TimFile" v1–v23 with a Vietnamese UI).
- Light UI mode (flat, no translucency) for weak GPUs and remote-desktop sessions; hover-static in light mode to minimize repaints.
- Fixed tables not stretching to the full window width when maximized (remainder column).
- Fixed single-file deletes from a Cleanup-tips card not removing the row from that card's detail list.
- Files already gone from disk are treated as handled (removed from the lists) instead of erroring with "os error 2"; small deletes now update all lists immediately.
- Persistent scan index and duplicate results per root, reloaded at startup; hash cache for near-instant duplicate re-runs; hardlink detection.
