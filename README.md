# projectmanagement-plugin-sicompass

*A kanban board, in Sicompass.*

This plugin is part of [Sicompass](https://github.com/friendlyflow/sicompass), a
keyboard-first, accessibility-first way to use your entire computer.

Project management holds a kanban board. The root lists the columns and each
column lists its cards, so the whole board is navigable as an ordinary list of
lists. Columns are added, renamed and reordered there, with the same keys as
everywhere else in Sicompass.

Press d to see it drawn as a board instead, with the columns side by side. The
cursor sits on a card, and the one card it is on is the only thing highlighted.
The editing keys are the ones you already know: i and a to edit the card, o to
open a new one, ctrl+d to delete, ctrl+x, ctrl+c and ctrl+v to cut, copy and
paste, and ctrl+z to undo. Escape takes you back to the list, onto the card you
were on.

The archive card command retires a card into an archive column. The board does
not draw it, and the list does. Move a card left to take it back out.

The first row of every list is its list meta. It shows a hash of the list
that changes whenever anything in it changes, the same kind of hash notes
have.

Your board is plain files in your Sicompass data folder, on your own
computer.

## Cloud sync

Cloud sync is off until you turn it on, in Settings, under project
management. With it on, a row above the columns says where your subscription
stands, and your board stays the same on every computer you turn it on for. A
few seconds after you stop editing, and once a minute otherwise, Sicompass
sends your changes to the Sicompass Cloud server and brings in the changes you
made elsewhere.

The hashes are how Sicompass and the server know which columns and cards are
out of date, and the list meta also says whether a list changed since the last
sync. When the same card was changed on two computers, the latest change is
kept. A card deleted on one computer and edited on another is kept.

Cloud sync is part of Sicompass Cloud, which you buy and redeem in the Store,
under tiers. Without it your board works exactly the same, and is only not
synced. After a subscription ends, sync keeps running for 14 more days.

On a new computer, turn cloud sync on, and your board arrives. Sync with the
cloud now, in the command palette, does it at once.

## Install

In Sicompass, open store, then programs, and press Enter on install next to
project management. The Store checks the release's signature before installing
it, and keeps it up to date.

## Building from source

```bash
nix develop          # the toolchain
cargo test           # the board, the list and the sync logic
cargo build --release
cp target/release/projectmanagement-plugin plugin
```

To install a build of your own, copy `plugin.json`, the built `plugin` program
(`plugin.exe` on Windows) and `locales/` into a folder named
`projectmanagement` in the Sicompass plugins folder
(`~/.config/sicompass/plugins/` on Linux, `~/Library/Application
Support/sicompass/plugins/` on macOS) and restart Sicompass.

`./scripts/release-plugin.sh --dry-run` builds this computer's release, packs
it, and signs and verifies it with a throwaway key, the way a release is made.

## Related repositories

- [sicompass](https://github.com/friendlyflow/sicompass), the application
- [sicompass-plugin-sdk](https://github.com/friendlyflow/sicompass-plugin-sdk),
  the SDK, the plugin kit and the cloud sync library (`sicompass-sync`)

## Community

Join the conversation on
[Discord](https://discord.com/channels/1464152138753249313/1464152139231137894).

## License

#### Open source license

If you are creating an open source application under a license compatible with
the GNU GPL license v3, you may use this project under the terms of the GPLv3.
See [LICENSE](LICENSE).

## Contributing

Contributions are welcome. Whether it is code, documentation, or feedback, your
input helps make computing more accessible for everyone.
