# About this repository

Upstream is [Pumpkin-MC/Pumpkin](https://github.com/Pumpkin-MC/Pumpkin), a
Minecraft server written in Rust, GPL-3.0. This repository carries that work
with a small set of changes on top, and merges upstream `master` on a schedule.

It is not a GitHub fork in the "Fork" button sense — there is no parent link —
so the relationship is stated here instead of by the UI.

## What it is for

Pumpkin was written to be a **process**. You launch it from a shell; it owns
stdout, it owns the working directory, and when it cannot start it exits.

Homerun Go runs it **inside a phone app**, where none of those are available:

- **iOS cannot spawn child processes at all.** The server has to be a library
  linked into the app binary, not something the app launches.
- **Neither platform surfaces stdout.** A server that logs there has no
  console, and the console is something players actually use.
- **`process::exit` takes the whole app down.** On a bind failure the player
  gets a dead app with no crash report and no way for the interface to explain
  what happened.

So the job of this repository is narrow, and worth stating as a constraint
rather than a goal: **make a program work as a library, without changing what
the server does.** Everything here should be reviewable against that sentence.

Nothing app-shaped is patched in; that stays in the embedder. Keeping this
repository to library-mode changes is what makes upstream merges routine, and
what might one day let these changes go upstream and this repository stop
existing.

## What is changed

| Where | Change | Why |
|---|---|---|
| `crates/pumpkin/src/lib.rs` | `PumpkinServer::new` returns `Result<Self, io::Error>` instead of calling `process::exit(1)` on a bind failure | The standalone binary still decides to exit — it just decides it itself. An embedder can report the error instead of dying. |
| `crates/pumpkin/src/log_ring.rs` | A bounded in-memory log ring | stdout is invisible on both mobile platforms, so the console has to be readable from memory. |
| `crates/pumpkin/src/ios.rs` | iOS entry points | — |
| `crates/pumpkin/src/plugin/` | The native (`dlopen`) plugin loader is compiled out on iOS; the WASM loader stays | iOS does not permit loading unsigned native code. |
| `plugin/loader/wasm/wasm_host/` | wasmtime on the Pulley interpreter | iOS does not permit JIT. |
| `reset_server_state()`, `STOP_INTERRUPT` as `ArcSwap` | Process-wide statics made safe to run twice | A second run in the same process saw the first run's stop request and exited immediately, which looks exactly like "the server won't start". |

## Building

Clone with submodules. `crates/pumpkin-plugin-wit` is one, and without it
`wit_bindgen::generate!` has no WIT definitions to read, which surfaces as
three hundred unresolved-import errors in `pumpkin-plugin-api` rather than as
anything mentioning submodules.

```bash
git clone --recurse-submodules https://github.com/hintjen/Pumpkin.git
# already cloned:
git submodule update --init --recursive
```

The checks this tree is expected to pass:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked
cargo test --workspace --locked
```

## Staying current with upstream

Upstream `master` is merged in on a schedule, gated on `fmt`, `clippy` and the
test suite before anything lands, and opened as a pull request rather than
pushed. The automation itself is not published here — it is specific to one
build host and of no use to anyone else.

Consumers should pin an exact revision rather than a branch. Upstream tracks
Minecraft protocol releases, and that churn should not arrive in someone's
build uninvited.

## Licence

GPL-3.0, inherited from upstream, with copyright held by Pumpkin's contributors.
See `LICENSE`.
