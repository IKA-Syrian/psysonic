# Psysonic (ayhamo fork)

This is [Psysonic](https://github.com/Psychotoxical/psysonic) with two small changes. The original README is [here](../README.md).

## What is different

- **Song covers:** everywhere a single song is shown (player bar, Now Playing, queue, fullscreen, song cards, track lists) it shows the song's own embedded cover (`mf-<songId>` on Navidrome) instead of the album cover. See `songOwnCoverEntry` (`src/cover/resolveEntry.ts`) and `usePlaybackTrackCoverRef`. Nine upstream cover tests that expect album covers fail on purpose; they are left untouched to avoid merge conflicts.
- **Crossfade on skip:** in classic crossfade mode, Next/Previous also fades over the crossfade length (`src/features/playback/utils/playback/autodjManualBlend.ts`).
- **Updates come from this fork:** the in-app update check reads this fork's releases (`src/features/updater`). `src-tauri/fork.conf.json` swaps in this fork's updater key and feed at build time.

## How releases happen

The **Fork sync and release** workflow runs every day. When upstream's `release` branch has a version this fork hasn't released, it merges that version into `covers` inside the runner (nothing is pushed), builds the Windows installer, signs it and publishes `fork-vX.Y.Z`. The app then offers the update. It can also be run by hand with **force** to rebuild the current version.

## If a run fails

Usually upstream changed the same lines as a patch. Merge `upstream/release` into `covers` locally, fix the conflict, push, then run the workflow by hand.

- **Branch:** `covers` is the default branch, based on upstream `release` (stable versions only).
- **Signing key:** repo secrets `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. The public half is in `src-tauri/fork.conf.json`. If the key is lost, generate a new one, update both places and reinstall once by hand, because installed copies only accept updates signed with the old key.
