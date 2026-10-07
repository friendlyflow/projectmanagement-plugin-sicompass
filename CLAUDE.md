# Project Instructions

projectmanagement-plugin-sicompass was split out of the
[sicompass](https://github.com/friendlyflow/sicompass) workspace, and its git
history before that point is the history of `lib/lib_project_management` there.
Work on it is usually driven from a sicompass checkout next to this one
(`../sicompass`), whose `/commit-and-push`, `/release`, `/sync` and
`/update-cargo` take this repo's name as their first argument and then follow
the skills in this repo's `.claude/skills/`.

It is a sicompass **plugin process**: a program (`src/main.rs`) built with the
SDK's `plugin` feature, which sicompass starts and talks to over its stdin and
stdout. It runs with the user's rights. The Store installs it from this repo's
GitHub releases, one build per platform. The plugin platform is described in
`../sicompass/docs/plugin-platform.md`.

- `plugin.json` is the manifest. Its `name` (`projectmanagement`, no space, so
  `displayName.replace(' ', "")` equals it) is also the install folder, the
  settings section and the storage folder, and its `version` must equal the
  release tag. Permissions, which declare what the plugin does and are shown
  to the user before install: `storage` (the board, in
  `sicompass_sdk::plugin::storage_dir()`, which is
  `<data dir>/projectmanagement`, the folder the old built-in used, so existing
  boards open unchanged) and `allowedHosts ["store.sicompass.org"]` (the
  sync server). `service.tier` is
  `friendlyflow/cloud`, which is what lets `license::token` hand this plugin
  the user's Sicompass Cloud redeem token.
- `locales/<lang>.ftl`, every id prefixed `projectmanagement-`, in all four
  languages. `src/localize.rs` asks the app (`host::translate`), and in the
  unit tests, which run outside sicompass, reads `en-US.ftl`, so they see the
  English text.
- `src/lib.rs` is the plugin (`ProjectManagementProvider`, `impl Plugin`):
  the list surface, the board dashboard and its keys, and board undo.
  `src/main.rs` makes it the program. `board.rs` is the model, `store.rs` the on-disk format, `render.rs` draws
  the board, `escape.rs` escapes every row.
- `src/cloud.rs` is the optional cloud sync (the server store is named
  `kanban`, settings key `kanbanCloudBackup`, both kept from when it was a
  backup), on the `sicompass-sync` library, with a host of its own (`PluginHost`): the app through the plugin
  kit, threads for the tasks, and `ureq` (rustls) for the HTTP.

## Two surfaces, one board

Read the module docs at the top of `src/lib.rs` before changing either
surface. In short:

- **List edits** ride the app's structural-edit capability. The app mutates its
  own tree, records the undo, and hands the list back through
  `sync_ffon_body_children`, which diffs by `<id>`.
- **Board edits** never touch the app's tree, so they are recorded here as
  `ProviderOp` entries (a JSON `BoardOp` in the payload) and reversed in
  `Plugin::undo`. `dashboard_uses_app_undo` puts them on the app's timeline.
- **`render.rs` draws in the SDK's dashboard types** (`DashboardFrame`,
  `DashboardPalette`), which its tests read. The plugin interface has its own
  wire types, so `to_sdk_palette` converts the palette on the way in and the
  SDK's `From<DashboardFrame>` the frame on the way out. Keys, dashboard
  requests and navigation requests use the interface's types throughout.
- **The archive is an ordinary column**, pinned last, which the board stops
  drawing one short of. `clamp_focus` is what keeps the board cursor off it.
- **Every list opens with its list meta**, a rendered `Obj` (never stored,
  never editable) holding the Merkle hash of the board or column and, with
  sync on, its sync status. The board view draws from `Board` and never sees
  it, but the app counts it: the dashboard entry path and `SelectPath` are
  `fetch()` row indices, so they go through `lead_rows`.

## Cloud sync: four things that are easy to get wrong

- **The hash is a wire format, and not this repo's.** `board.rs` applies
  `sicompass_sync::merkle`'s formula (the notes plugin's), which the server and
  every other computer share. The saved store must stay byte for byte its
  canonical form (`merkle::to_files`), which a test checks: otherwise every
  sync uploads a rewrite of it.
- **The paywall is on the service, never on the data.** Whatever
  `license::standing` says, the board is listed and saved to disk. Only the
  sync is gated (active or grace).
- **The sync row and the list meta are rendered, never stored.** The sync row
  carries `<id>cloud</id>`, the meta row is matched by its localized label, and
  `reconcile` skips both. The app hands back whatever it displayed, so without
  that either becomes a column. The sync row never links anywhere: buying and
  redeeming are in the Store, under tiers.
- **Nothing slow runs on the calls from the app.** The app waits for every
  call to answer. `persist` only marks the debounce. `poll` starts a sync task
  at start-up, once the board is quiet, and every minute. A task runs on a
  thread of its own (`PluginHost::spawn`), with only the board's folder on
  disk and the token. `poll` hands its result to
  `ProjectManagementProvider::task_done`, where what another computer changed
  is written and the board read again, unless the board was saved meanwhile
  (then the next sync merges again).

## Environment (Nix)

The toolchain comes from the flake dev shell in [flake.nix](flake.nix): Rust
from rust-overlay with this computer's plugin target (static musl on Linux,
which nixpkgs' rustc has no std for) and `jq`. Nothing is installed
system-wide.

- **Check once per session**, then stick with the answer: `command -v cargo`.
  - Non-empty: the shell is inside `nix develop`, so run `cargo ...` directly.
  - Empty: prefix every toolchain command with `nix develop -c`.
- `nix develop -c <cmd>` prints a `warning: Git tree ... is dirty` line on
  stderr first. That warning is noise, not a failure.
- Evaluate the flake through `git+file://$PWD`, never a plain path (a plain path
  copies `target/` into the store and hangs), and always under `timeout`.
- The version lives in `plugin.json` and in `[package] version` in `Cargo.toml`.
  Bump both together.

## Generated files that are committed

- `THIRD-PARTY-LICENSES.html`: `cargo about generate about.hbs -o
  THIRD-PARTY-LICENSES.html` (cargo-about 0.9.2, the version the `licenses.yml`
  workflow pins). Regenerate and commit it with any dependency change. The
  workflow fails if it drifts.

## Code Style

Follow standard Rust idioms. Use `#[allow(...)]` sparingly and only when
justified. In `README.md`, do not use em dashes or semicolons. Use commas
instead, or split into separate sentences.

## Testing

- After implementing changes, always run the tests before finishing:
  `cargo test`, and `./scripts/release-plugin.sh --dry-run`, which also builds
  this computer's release and verifies it the way the Store will.
- When adding new code, write or update tests.
- If tests fail, fix the code. Never leave a task with failing tests.

## Test Integrity

- Never remove or weaken test assertions to make a failing test pass. Fix the
  code instead.
- If a test itself is genuinely wrong and needs changing, **ask the user
  first** before modifying it.

## Releasing

A release is a `vX.Y.Z` tag on `main`, equal to `plugin.json`'s version. See
`.claude/skills/release/SKILL.md`. Before tagging, run
`nix develop -c ./scripts/release-plugin.sh --dry-run` (needs the
`sicompass-plugin` tool: `cargo install --git
https://github.com/friendlyflow/sicompass-plugin-sdk sicompass-plugin`). The
release workflow signs with the `PLUGIN_SIGNING_KEY` secret and checks it
against the `PLUGIN_PUBLIC_KEY` variable, the key the sicompass store list
names. The secret key file is `~/.config/sicompass/plugin-keys/projectmanagement.key`
on the maintainer's machine. Never print, copy or commit it.

The SDK comes from crates.io, and `sicompass-sync` by git at the SDK's
release tag (the source is all in `../sicompass-plugin-sdk`). The
commented-out `[patch]` in `Cargo.toml` is for working on them together, and
stays commented on main.

A release has one archive per platform. The release workflow builds them on
five runners (Linux x86_64 and arm64 as static musl, macOS arm64 and x86_64,
Windows x86_64), then packs, signs and verifies them in one job.
