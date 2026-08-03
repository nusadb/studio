# NusaDB Studio

Native desktop database-management GUI for NusaDB — **Rust + Slint** (pure-native, GPU-rendered,
no web/webview). A network client that connects to **multiple NusaDB servers** over the wire
protocol via the native driver `nusadb-libnusa`.

Design intent, screen inventory, and the phased build plan (G1–G10) live in
[`info/Plan/19_gui_studio.md`](../../info/Plan/19_gui_studio.md). Visual reference (Figma export):
[`gui/desain/`](../desain) — UX is a living reference and may still change.

This is a **standalone Cargo workspace** (excluded from the engine workspace) so its GUI deps do
not bloat the engine build.

## Run (development)

```
cargo run                     # opens the native NusaDB Studio window
```

- **Live-preview the UI** without recompiling: install the **Slint** VS Code extension, open a
  `.slint` file, and use its preview panel. CLI: `cargo install slint-viewer` then
  `slint-viewer ui/app.slint`.
- **Live-reload** at runtime (debug): `SLINT_LIVE_RELOAD=1 cargo run`.

## Status

- **G1 — scaffold (this):** window shell + design palette. Runs, themed dark.
- Next: G2 driver wiring (`nusadb-libnusa`) · G3 multi-server navigator · G4 query editor + grid.

Connect target defaults: `127.0.0.1:5678` · database `nusadb` · user `nusa-root`.
