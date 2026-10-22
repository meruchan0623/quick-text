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
The corresponding source of a release is two archives, published together:
`Quicker-Text-<version>-Source.zip` (all workspace code, native vendor code, Cargo.lock,
frontend lockfile and build/package scripts needed to reproduce it) and
`Quicker-Text-<version>-Source-deps.zip` (the source of every third-party package built into
the binaries, see below).

## Rust and npm dependencies

The executable also contains crates from crates.io, and the frontend bundle contains the
runtime npm packages listed in `package-lock.json` (React, React DOM, scheduler and
`@tauri-apps/api`, plus the module-preload polyfill that Vite embeds in every production
bundle). They are used under their own licenses (MIT, Apache-2.0, BSD-style and similar
permissive licenses; the exact allowlist is in `deny.toml`), all compatible with
GPL-3.0-or-later.

Their copyright notices and license texts travel with every release artifact in
`THIRD-PARTY-LICENSES.txt`: it is bundled into the installer and the portable package
(next to the executable, together with the GPL text and this file) and attached to the
release as `Quicker-Text-<version>-THIRD-PARTY-LICENSES.txt`. The file is not kept in the
repository: `scripts/generate-third-party-licenses.ps1` writes it from the packages that are
actually distributed (the normal and build dependencies of the application crate for the
Windows x64 target from `cargo metadata --locked`, and the production packages of
`package-lock.json`) and fails when any package has no license text, so a release cannot be
packaged with an incomplete file. A few crates publish no license file in their source
package; for those the file reproduces the upstream project's text, pinned by name and
version in `scripts/third-party-license-clarifications.json` and marked `clarified`.

The source of these packages is published with every release as
`Quicker-Text-<version>-Source-deps.zip` (`cargo vendor --locked` output plus the npm
production packages, with offline build instructions); the portable package carries it as a
companion file next to the ZIP, named and hashed in the ZIP's `DEPENDENCY-SOURCES.txt`. The
build and packaging scripts (installer build, release package, portable package) fail when
the archive is missing or does not match the lock files, so no binary is produced without
it. Exact versions and checksums are also
pinned in `Cargo.lock` and `package-lock.json`. `src-tauri/resources/THIRD-PARTY-LICENSES.txt`
in the repository is only a placeholder that keeps the bundle configuration valid; the
release workflow replaces it for the installer build and restores it afterwards.

The licenses of the Rust crates are checked with `cargo deny` (`deny.toml`) in CI and before
every release. The licenses of the runtime npm packages are checked with the release
checklist (`docs/release-checklist.md`).

The license statement and source-acquisition instructions for the already published
release 0.3.1 are in `docs/licensing-0.3.1.md`.
