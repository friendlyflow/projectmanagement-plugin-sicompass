# Changelog

## Unreleased

- The first row of every list is called header now (it was list meta). In Dutch it is kop, in French en-tête and in German Kopfzeile.
- Each list's file on disk is `.header` now. A `.listmeta` from before is still read, and replaced on the next save or sync.

## 0.4.0

The board is a Merkle tree now, like the notes, and cloud backup is cloud sync.

- Every card, column and the board itself has a hash. The first row of the columns, and of every column, is its list meta, showing that hash and, with cloud sync on, whether the list changed since the last sync.
- A few seconds after you stop editing, once a minute, and when you choose sync with the cloud now, your changes go to Sicompass Cloud and the changes you made on your other computers come in. On a new computer, turning sync on brings your board in, so the restore command is gone.
- When the same card was changed on two computers, the latest change is kept, and a card deleted on one computer and edited on another is kept.
- The switch in Settings is worded enable cloud sync, and stays on if you had cloud backup on.
- Fixed: with the cloud row showing, going between the list and the board landed one row off.

## 0.3.0

Project Management is a program of its own now, instead of a sandboxed WebAssembly component.
Sicompass starts it and talks to it, one per tab, and it runs with your rights, so
its entry in the Store says what it does before you install it, and installing it is
your approval.

- Your board stays in the same folder. Backup to Sicompass Cloud works as before, and your board is shown and saved whatever your subscription says.
- One build for each of Linux (x86_64 and arm64, static), macOS (Apple Silicon and
  Intel) and Windows.
- Needs a Sicompass that runs plugin programs. An older Sicompass keeps the 0.2
  version it has.
