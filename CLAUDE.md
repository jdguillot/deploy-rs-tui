# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust + ratatui terminal UI that wraps [serokell/deploy-rs](https://github.com/serokell/deploy-rs).
It does not reimplement deploy-rs — it shells out to the `deploy` binary
and to `nix` / `ssh`. The repo is small enough to keep flat under `src/`.

## Common commands

All of these assume you're inside the dev shell (`nix develop`) so that
`cargo`, `rustc`, `deploy`, `nix`, and `ssh` are on `PATH`.

| task                  | command                                       |
| --------------------- | --------------------------------------------- |
| dev shell             | `nix develop`                                 |
| build                 | `cargo build`                                 |
| release build         | `cargo build --release`                       |
| run against a flake   | `cargo run -- /path/to/flake`                 |
| run against cwd       | `cargo run`                                   |
| lint                  | `cargo clippy --all-targets -- -D warnings`   |
| format                | `cargo fmt`                                   |
| nix build             | `nix build`                                   |

There are no tests yet. When adding any, prefer integration-style tests
that mock `nix` / `deploy` / `ssh` via `PATH` shims rather than unit
tests over the wrapper functions — the wrapper functions are very thin.

## Architecture

The flow is `flake → nodes → status → user action → deploy`.

```
                ┌─────────────┐
                │   main.rs   │  parse CLI, init tracing, init terminal
                └──────┬──────┘
                       ▼
                ┌─────────────┐
                │  flake.rs   │  `nix eval --json` of deploy.nodes
                └──────┬──────┘
                       ▼
                ┌─────────────┐
                │   app.rs    │  state, input modes, tokio::select! loop
                └──┬───┬───┬──┘
        events     │   │   │   background tasks
        ┌──────────┘   │   └───────────────┐
        ▼              ▼                   ▼
   ┌─────────┐   ┌──────────┐         ┌──────────┐
   │event.rs │   │ host.rs  │         │deploy.rs │
   │keys+tick│   │ tcp +    │         │spawns    │
   │         │   │ ssh+nix  │         │`deploy`  │
   └─────────┘   └────┬─────┘         └────┬─────┘
                      └──────┬──────┬──────┘
                             ▼      ▼
                          ┌──────────┐
                          │ ssh.rs   │  SshOverride struct shared by
                          │          │  status checks + deploy runner
                          └──────────┘
        │
        ▼
   ┌─────────┐
   │  ui.rs  │  ratatui rendering (incl. modal + popup)
   └─────────┘
```

Key invariants worth knowing before touching the code:

- **`flake::discover` is shallow on purpose.** It applies a Nix function
  that strips `path` from each profile, so we don't force evaluation of
  every NixOS module just to draw the host list. If you add a field to
  `Node`/`Profile`, also add it to the `--apply` expression in
  `flake.rs`.
- **`host::check_online` is the only "always-on" background work.** It
  runs once at startup and again on every `r` keypress. Everything more
  expensive (`u`, deploy itself) is lazy and user-triggered.
- **`host::check_profile_up_to_date` resolves the deploy-rs wrapper
  to its toplevel** before comparing against the remote's
  `/run/current-system`. Stripping `/activate` alone isn't enough —
  that yields the activation *wrapper* (e.g.
  `…-nixos-system-<host>-…-activate-path`), whose store hash differs
  from the toplevel (`…-nixos-system-<host>-…`) the remote symlink
  actually points at. The wrapper's direct references include the
  toplevel; `resolve_local_toplevel_quiet` picks it out by parsed
  name match. The fallback `parsed_paths_equivalent` compares
  `<name, version>` pairs when the wrapper isn't in the local store.
- **`app::App::run` is one `tokio::select!`** over three sources: term
  events, background status updates, and live deploy log lines. The
  optional deploy receiver is handled with `recv_optional`, which yields
  a never-resolving future when the receiver is `None` so the `select!`
  arm just stays pending.
- **The deploy log is the only mutable buffer that grows.** It's capped
  at 2000 lines in `App::push_log`. If you add other long-lived buffers,
  cap them too.
- **Modes map directly to deploy-rs flags:**
  `Switch` → no flag, `Boot` → `--boot`, `DryRun` → `--dry-activate`.
  Don't try to emulate `Boot` by SSH-ing manually — deploy-rs already
  supports it.
- **Toggles only emit a flag when they differ from the deploy-rs
  default.** This is on purpose: the flake's `deploy.nodes` settings
  stay authoritative until the user actively flips a switch. If you add
  a toggle, decide its default to match deploy-rs and follow the same
  "only-emit-if-changed" rule in `deploy::run_inner`.
- **`SshOverride` is the single source of truth** for both status
  checks (`host::ssh_capture`) and the deploy runner. If you add a new
  field, update *both* `ssh_args()` (per-token argv for ssh) and
  `deploy_ssh_opts()` (joined string for `--ssh-opts`).
- **The host-list `[ssh]` marker is driven by `SshOverride::is_active`.**
  When clearing the last field of an override, also remove the entry
  from `App.overrides` so the marker disappears — this is what
  `handle_key_edit_override` does.
- **App input mode is a state machine, not just a flag.** Key dispatch
  in `app::App::handle_key` first short-circuits Ctrl-C and the help
  popup, then routes by `InputMode`. Adding a new modal mode means
  adding a new variant *and* a new dispatch arm.
- **`kill_on_drop(true)`** is set on the spawned `deploy` Command so
  cancelling (key `x`) actually reaps the child instead of orphaning
  it. Don't remove it.
- **`NO_COLOR=1`** is set on the spawned `deploy` so its output stays
  legible when forwarded line-by-line into ratatui.
- **`--interactive-sudo true` will hang the TUI**, by design — the
  child reads from `Stdio::null()`. Toggle 5 is exposed for
  completeness; the help popup tells the user to press `x` to recover.

## Project conventions

- The project shells out heavily. Treat `nix`, `deploy`, and `ssh` as
  load-bearing dependencies — every code path that touches them should
  surface stderr to the user, not swallow it.
- Errors that originate from external tools should be wrapped with
  `anyhow::Context` describing *what we were doing*, not *what tool we
  ran* (e.g. `discovering deploy.nodes`, not `running nix eval`).
- Don't print to stdout/stderr from the main thread once the TUI is up
  — it will corrupt the alternate screen. Use `--log-file` and tracing
  if you need diagnostics.
- The host badges (`sys:✓` / `sys:↑` / `sys:!` / `sys:?` / `sys:-`) and
  the colors are part of the user-facing contract — see README. Keep
  them consistent if you change rendering.
