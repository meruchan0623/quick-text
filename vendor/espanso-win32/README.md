# Espanso Windows native input

Source: https://github.com/espanso/espanso/tree/a87d1723f47a59e38d370e2f0c636d66d568b8f6 (v2.4.1).
Copied espanso-detect/src/win32/native.{cpp,h} and espanso-inject/src/win32/native.{cpp,h}.
Copyright (C) 2019-2021 Federico Terzi. GPL-3.0-or-later; complete upstream license in LICENSE.

Local changes to injector: return accepted SendInput event counts, mark every event
with Quicktext's 0x51545854 tag, and stop delayed sequences on failed submission.
Algorithms and UTF-16/key ordering remain upstream's. Detector locally removes RIDEV_NOLEGACY because it shares the GUI process (Espanso uses a separate worker), and handles GetMessage failure without dispatching stale data.
The Rust bridge, matching, cancellation and protection rules live in crates/platform.

Patch date: 2026-10-15. Modified upstream files: `detect/native.cpp`,
`inject/native.cpp`, `inject/native.h`; `detect/native.h` is unchanged.
