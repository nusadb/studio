# NusaDB Studio

A desktop client for NusaDB, written in Rust with Slint. It renders on the GPU and does not
embed a browser. One window can hold connections to several servers at once, and it talks to
them through the published Rust driver [`nusadb`](https://crates.io/crates/nusadb).

## What it does

**Servers.** Saved servers appear as cards you can connect, disconnect, edit, test and remove
(removal takes two steps). Each can carry an environment tag, and a server tagged production is
named in the top bar, not only coloured. TLS needs a CA bundle; there is no setting that quietly
falls back to plaintext. The list is saved to `<config dir>/nusadb-studio/servers.json`, which
never holds a password. By default a password lives in memory for the session. With **Remember
password** it goes to the operating system's keychain (Keychain on macOS, Credential Manager on
Windows, Secret Service such as GNOME Keyring or KWallet on Linux) under the service
`nusadb-studio` and the account `user@host:port`. When no keychain is available, Studio says so
and keeps the password for the session only.

**Navigator.** Server, database, schema, then tables, views, sequences and functions, loaded
lazily from the server's own catalogs. The filter searches every loaded level and opens the path
to each match. Right-click a row for its actions: browse, details, query, import, edit structure,
privileges, maintenance (ANALYZE, VACUUM, REINDEX, CHECKPOINT), new database, schema, table or
view, rename, duplicate, export as SQL, truncate and drop. The arrow keys move through the tree
and Enter opens a row.

**Query.** Each tab keeps its own script, target and results. The editor highlights SQL as you
type, numbers its lines, and completes keywords, tables and columns, also through aliases
(`o.am` completes to `amount`). Run executes the selection or the statement at the cursor,
Stop cancels on the server, Explain shows the plan as an operator tree with estimates, and
Begin, Commit and Rollback show the session's transaction state. A script runs statement by
statement and stops at the first error. Results come with Messages (with SQLSTATE), JSON, Plan
and History tabs; history keeps the last 100 scripts. Find and replace, a formatter that only
changes whitespace and keyword case, and opening and saving `.sql` files are in the editor too.
A result read by a plain `SELECT` from one table that includes the primary key can be edited in
place, like the Data grid.

**Data.** A table or view pages 200 rows at a time with its total count, narrows with a WHERE
clause, and opens the same query in the editor. Tables with a primary key edit like a
spreadsheet. Changes are staged and tinted until you save them, and saving applies all of them
in one transaction or none of them, with the server's message when a value is refused. Leaving
with unsaved changes is refused rather than silently discarding them. Import loads CSV,
delimited text or a JSON array through one `COPY ... FROM STDIN`, after showing how the file's
columns map to the table's.

**Design and diagram.** The designer creates or changes a table (columns, types, NOT NULL,
defaults, primary key, indexes including unique, multi-column and partial ones) and shows the
exact SQL as you edit. It runs only the `ALTER TABLE`, `CREATE INDEX` and `DROP INDEX` the change
needs, in one transaction. The diagram draws a database's tables and foreign keys: referenced
tables to the left of the tables that reference them, primary and foreign key marks, crow's feet
on the many side. You can drag, zoom, find a table and export the diagram as SVG.

**Objects.** Double-click an object for its page: data, columns, constraints, indexes, triggers,
policies and a DDL script that recreates it. The inspector beside the editor summarises whatever
is selected.

**Monitor.** Reads a server's metrics endpoint (`--metrics-listen`) every two seconds while the
screen is open: active connections, statements, errors and new connections per second with two
minutes of history, each database's state, and scheduled backups. It also lists prepared
transactions and can commit or roll them back.

**Security.** Roles and their attributes, memberships, a privilege matrix per role over every
table and view, and row-level security with policies edited in place. Edits are staged, the
GRANT, REVOKE, ALTER ROLE and POLICY script is shown as you go, and Apply runs it in one
transaction. Without a superuser connection the superuser-only parts are read-only.

**Analysis.** Query results have a Chart tab (bar, line, scatter, histogram, pie) and a Pivot tab
with totals. Selected cells show count, sum, average, minimum and maximum in the status bar. A
table or view has a Profile tab with per-column statistics, common values, histograms and
correlations. A table with a `VECTOR` column gets a Vectors tab: its HNSW indexes, nearest
neighbour search with a recall check against an exact search, hybrid vector and full-text
search, and a 2D map of a sample.

**Notifications.** A tab that listens on a connection of its own, so the editor's session and
transaction are untouched, and sends NOTIFY through the editor's session.

**Export.** Copy rows or whole results as tab-separated text, or export to CSV, Excel, JSON, JSON
Lines, TSV, Markdown or SQL `INSERT` statements. Exports stream from the server, so they hold
every row rather than the 1,000 the grid keeps. Export as SQL on a database or schema writes one
file that recreates its schemas, sequences, tables in foreign-key order, rows and views.

**Confirmation.** Studio asks first only where a mistake cannot be taken back: DROP, TRUNCATE,
DELETE or UPDATE without WHERE, any change on a server tagged production, applying structure or
privilege changes, finishing a prepared transaction, and closing or disconnecting while a
transaction is open or work is unsaved.

Some things are left out because the server does not expose them yet, such as a partial index's
WHERE clause, column-level grants and table sizes. Studio shows nothing rather than a guess.
`CLUSTER` is not offered, since NusaDB accepts it for compatibility and does nothing.

## Building and running

The driver comes from crates.io, so a plain checkout builds on its own:

```
cargo run
```

To try a connection, start a server and point the saved `local` entry at it:

```
docker run -d --name nusadb-dev -p 5678:5678 nusadb/nusadb:latest
```

The form's defaults (127.0.0.1, port 5678, user `nusadb-root`, database `nusadb`) match that
container.

While working on the interface you can preview a `.slint` file without recompiling with the
Slint extension for VS Code, or with `slint-viewer ui/app.slint`. Setting `SLINT_LIVE_RELOAD=1`
makes a debug build re-read the files as you edit them.

A `.sql` path given on the command line opens in its own Query tab, which is how "Open with"
works from a file manager.

## Packages

`cargo build --release` gives an optimised, stripped binary. Each platform's packages are built
on that platform and land in `dist/`:

| Platform | Command | Produces |
|---|---|---|
| Linux | `sh packaging/linux/build-packages.sh` | a `.deb`, and a `.tar.gz` whose `install.sh` installs into `~/.local` without root |
| macOS | `sh packaging/macos/build-dmg.sh` | a universal `NusaDB Studio.app` (Apple silicon and Intel, macOS 11 or later) in a `.dmg` |
| Windows | `powershell -ExecutionPolicy Bypass -File packaging\windows\build-installer.ps1`, or `sh packaging/windows/build-installer.sh` from Linux with MinGW-w64 and NSIS | a per-user installer that needs no administrator rights, and a portable `.zip` |

On macOS the app is ad-hoc signed unless `CODESIGN_IDENTITY` names a Developer ID certificate;
add `NOTARY_PROFILE` to notarise it. The icons are in `packaging/icons/`.
`packaging/ci/studio-release.yml` is a GitHub Actions workflow that runs the checks and builds
all three platforms; copy it to `.github/workflows/` to use it. A `studio-v*` tag attaches the
packages to that tag's release.

## Keyboard shortcuts

The primary modifier is ⌘ on macOS and Ctrl elsewhere. Letters work with Caps Lock on, and the
Windows or Super key is never part of a shortcut. Tooltips show each platform's own notation.

| Action | Windows / Linux | macOS |
|---|---|---|
| Run the selection, or the statement at the cursor | Ctrl+Enter | ⌘↩ |
| Run the whole script | Ctrl+Shift+Enter, F5 | ⇧⌘↩, F5 |
| Completion | Ctrl+Space | ⌃Space |
| New / close query tab | Ctrl+T / Ctrl+W | ⌘T / ⌘W |
| Open / save / save as | Ctrl+O / Ctrl+S / Ctrl+Shift+S | ⌘O / ⌘S / ⇧⌘S |
| Find / replace | Ctrl+F / Ctrl+H | ⌘F / ⌥⌘F |
| Format SQL | Ctrl+Shift+F | ⇧⌘F |
| Grid: save staged edits | Ctrl+S | ⌘S |
| Grid: copy / paste cells | Ctrl+C / Ctrl+V | ⌘C / ⌘V |
| Grid: undo / redo a staged edit | Ctrl+Z / Ctrl+Y, Ctrl+Shift+Z | ⌘Z / ⇧⌘Z |
| Grid: edit a cell | Enter, F2, typing, double-click | the same |
| Grid: keep / drop an edit | Enter / Esc | the same |
| Value panel: apply | Ctrl+Enter | ⌘↩ |

Settings (the gear at the bottom of the rail) hold the theme, the editor font size, rows per page
and the shortcut list, saved in `settings.json` beside the server list.

## Source layout

```
ui/
  theme.slint       colours, spacing, radii, type sizes
  models.slint      structs that cross the Rust and Slint boundary
  components/       icon, button, text field, SQL editor, result grid, tooltip and the rest
  views/            one file per screen and per piece of the window's frame
  fonts/            Inter and JetBrains Mono NL, embedded, with their SIL OFL licences
  app.slint         the window itself, composition only
src/
  model.rs          domain types and form validation
  store.rs          saved servers and servers.json
  backend.rs        the interface live sessions go through
  driver.rs         that interface over the nusadb crate
  controller.rs     binds the window to everything else (screen parts in controller/)
  main.rs           startup
```

The remaining modules each hold one piece of logic with its tests, for example `sql_split.rs`
(cutting a script into statements), `complete.rs` (completion), `edits.rs` (the statements a
grid save runs), `designer.rs`, `security.rs`, `erd.rs`, `import.rs`, `export.rs` and
`risk.rs` (which scripts must be confirmed).

Three habits keep this workable as it grows. Views never name a colour or a size; they read from
`Theme` and `Metrics`, so a change to the design is a change to one file. Only the controller
imports both the generated Slint bindings and the domain modules, so the rest can be tested
without opening a window. Icons are vector paths in `components/icon.slint` rather than image
files or a font, so there is no asset pipeline to maintain.

## Notes on Slint

Inside a layout, `width` and `height` are suggestions. Only `min-` and `max-` bind, so every pane
that must keep its size clamps itself with both.

A child with no `x` or `y` inside a non-layout parent is centred rather than placed at the
origin, so every hairline states its position.

A pane may not choose its children from its own width, because its width is computed from its
children. The window decides the responsive tier once and passes it down.

## Checks

```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

The keychain round trip is an ignored test; run it with `cargo test -- --ignored` inside an
unlocked keychain session.

## Licence

Apache-2.0.
