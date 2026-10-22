# Third-party notices

## Espanso Windows detector and injector

`vendor/espanso-win32/{detect,inject}/native.{cpp,h}` is copied from Espanso v2.4.1,
commit `a87d1723f47a59e38d370e2f0c636d66d568b8f6`:
https://github.com/espanso/espanso

Copyright (C) 2019-2021 Federico Terzi. Licensed under GNU GPL version 3 or later.
The full license is at `vendor/espanso-win32/LICENSE`. Original copyright/license
headers are retained. Local modifications are documented in that directory's README.

Quicker Text itself is licensed under GPL-3.0-or-later (see `LICENSE`). The combined
executable includes GPL-3.0-or-later code and must be distributed with GPL rights,
the full license, and complete corresponding source for the combined work.
The source distribution must include all workspace code, native vendor code,
Cargo.lock, frontend lockfile and build/package scripts needed to reproduce it.

## Rust and npm dependencies

The executable also contains crates from crates.io, and the frontend bundle contains the
runtime npm packages listed in `package-lock.json` (React, React DOM, scheduler and
`@tauri-apps/api`). They are used under their own licenses (MIT, Apache-2.0, BSD-style and
similar permissive licenses; the exact allowlist is in `deny.toml`), all compatible with
GPL-3.0-or-later. Their copyright and license notices are part of their published source
packages on crates.io and npm. Exact versions and checksums are pinned in `Cargo.lock` and
`package-lock.json`; these packages are not copied into the source archive.

The licenses of the Rust crates are checked with `cargo deny` (`deny.toml`) in CI and before
every release. The licenses of the runtime npm packages are checked with the release
checklist (`docs/release-checklist.md`).

The license statement and source-acquisition instructions for the already published
release 0.3.1 are in `docs/licensing-0.3.1.md`.
