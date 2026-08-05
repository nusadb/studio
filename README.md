# NusaDB Studio

A desktop client for NusaDB, written in Rust with Slint. It renders on the GPU and does not
embed a browser. One window can hold connections to several servers at once.

## Status

Early. The application shell is complete and the connection path works end to end.

What you can do today: save servers, edit them, search them, and test a connection. A test
opens a real session through the driver, runs one trivial query to confirm the database
answers, then disconnects. When it fails, the reason the driver gave is what you see.

What is not there yet: the object tree stops at the server level, the SQL editor is read-only,
and queries do not execute. Screens reachable from the icon rail that have no implementation
say so instead of showing an empty panel.

Nothing in the interface is filled with sample data. The tree lists servers you actually saved,
and the result grid stays empty until a query has run. That seemed worth being strict about in
a database tool, since invented rows are indistinguishable from real ones.

## Building

The driver crate comes from the engine repository as a git dependency, so a normal build works
with no extra setup:

```
cargo run
```

To try a connection, start a server and point the saved `local` entry at it:

```
docker run -d --name nusadb-dev -p 5678:5678 nusadb/nusadb:latest
```

The defaults in the form (127.0.0.1, port 5678, user `nusa-root`, database `nusadb`) match that
container.

While working on the interface you can preview a `.slint` file without recompiling by
installing the Slint extension for VS Code, or by running `slint-viewer ui/app.slint`. Setting
`SLINT_LIVE_RELOAD=1` makes a debug build re-read the files as you edit them.

## Source layout

```
ui/
  theme.slint       colours, spacing, radii, type sizes
  models.slint      structs that cross the Rust and Slint boundary
  components/       icon, status dot, badge, button, text field, form field, toggle
  views/            top bar, icon rail, object tree, workspace, inspector,
                    connection manager, status bar
  app.slint         the window itself, composition only
src/
  model.rs          domain types and form validation
  store.rs          saved servers and the identities the interface addresses them by
  tree.rs           the object hierarchy, flattened into rows
  probe.rs          the interface a connection test goes through
  driver.rs         the implementation of that interface, over the wire protocol
  sql_highlight.rs  token classification for the editor
  controller.rs     binds the window to everything above
  ui.rs             bindings generated from ui/app.slint
  main.rs           startup
```

Three habits keep this workable as it grows.

Views never name a colour or a size. They read from `Theme` and `Metrics`, so a change to the
design is a change to one file.

Only `controller.rs` imports both the generated Slint bindings and the domain modules. Because
`model`, `store`, `tree` and `probe` know nothing about the toolkit, they can be tested without
opening a window. `cargo test` runs 35 of those.

Icons are vector paths in `components/icon.slint` rather than image files or a font, so there
is no asset pipeline to maintain.

## Notes on Slint

Two rules cost some time to discover and are worth writing down.

Inside a layout, `width` and `height` are suggestions. Only `min-` and `max-` bind. A plain
`height:` on a layout child gets renegotiated, which once left the status bar with no height at
all.

A child with no `x` or `y` inside a non-layout parent is centred rather than placed at the
origin. Every hairline here states its position, because skipping that drew the inspector's
left border down the middle of the pane.

## Checks

```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Licence

Apache-2.0.
