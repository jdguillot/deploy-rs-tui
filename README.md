# deploy-rs-tui

A small terminal UI on top of [serokell/deploy-rs](https://github.com/serokell/deploy-rs).
It reads `deploy.nodes` from your flake, shows which hosts are reachable
and (on demand) which ones are running stale builds, and lets you push
NixOS host configs, home-manager configs, or both — either as an
immediate switch or as a new boot entry for next boot.

## Features

- Auto-discovers every entry in `deploy.nodes` from a flake.
- Per-host **online/offline** indicator (TCP probe of port 22, no ICMP /
  no sudo required).
- On-demand **update check** (`u`) that compares the locally-built
  profile store path against the remote machine's `/run/current-system`
  (system) and home-manager profile (home). Correctly resolves the
  deploy-rs activation wrapper to its toplevel so hosts that are already
  current show `✓` rather than a spurious `↑`.
- **Closure size delta + package diff** (`Shift+U`) — measures the
  local vs. remote closure sizes and then automatically runs a
  metadata-only package-version diff (no heavy `nix copy`). Detects
  content-only changes (e.g. config file edits that don't bump any
  package version) and surfaces them distinctly.
- **Multi-host operations** — mark multiple hosts with `Space`, then
  `u` / `Shift+U` / `s` / `b` / `d` operate on all marked hosts at
  once.
- Choose what to deploy: **all profiles** / **system only** / **home only**.
- Choose how to deploy: **switch** (immediate), **boot** (next boot),
  or **dry-run** (`deploy --dry-activate`, build + diff only).
- **Job log pane** with live, line-buffered, ANSI-stripped `deploy`
  output. Each host gets a coloured prefix for legible batch output.
  Cancelling kills the child cleanly.
- **Log search** (`/`) with `n`/`N` navigation, `[current/total]`
  counter in the pane title, and a distinct cyan highlight on the active
  match.
- **Per-host SSH overrides** — for nodes that aren't in your
  `~/.ssh/config`, set hostname/IP, ssh user, identity file, and extra
  `-o` options from inside the TUI. Hosts with overrides show a magenta
  `[ssh]` tag in the list.
- **Toggles** for the deploy-rs flags you reach for most:
  `--skip-checks`, `--magic-rollback`, `--auto-rollback`,
  `--remote-build`, `--interactive-sudo`. Always-visible state strip.
- **Pane-jump keys** (`f`/`i`/`v`/`t`/`c`) for instant focus on any
  pane; `Tab`/`Shift+Tab` for sequential cycling.
- **Help popup** (`?`) with a full guide to every key, badge, and toggle.

## Requirements

- A flake that defines `deploy.nodes` in the style described in the
  [deploy-rs README](https://github.com/serokell/deploy-rs#overall-usage).
- `nix`, `deploy` (from deploy-rs), and `ssh` on `PATH` — the dev shell
  in this repo provides them.
- SSH access to your hosts using key auth (`BatchMode=yes` is set, so
  password prompts will fail fast).

## Building

This project lives in a Nix flake. The dev shell installs the Rust
toolchain plus everything the TUI shells out to:

```sh
nix develop
cargo build --release
```

Or build directly via Nix:

```sh
nix build
./result/bin/deploy-rs-tui /path/to/your/flake
```

## Running

```sh
# defaults to the current directory
deploy-rs-tui

# or point at any flake reference nix understands
deploy-rs-tui /home/me/.dotfiles
deploy-rs-tui github:me/dotfiles
```

Optional flags:

| flag         | purpose                                          |
| ------------ | ------------------------------------------------ |
| `--log-file` | write tracing logs to a file (TUI stays clean)   |

## Key bindings

| key            | action                                                       |
| -------------- | ------------------------------------------------------------ |
| `?`            | open the in-app help popup (full reference)                  |
| `q` / `Esc`    | quit (Esc also clears active search)                         |
| `Ctrl-C`       | quit (also cancels any running deploy)                       |
| `j` / `k`      | move selection / scroll log                                  |
| `g` / `G`      | jump to top / snap to tail                                   |
| `Space`        | mark/unmark host for batch operations                        |
| `Tab` / `Shift+Tab` | cycle focus forward / backward                          |
| `f`/`i`/`v`/`t`/`c` | jump to hosts / details / job log / toggles / commands  |
| `r`            | refresh online/offline for every host                        |
| `u`            | cheap-tier update check (paths + activation time)            |
| `Shift+U`      | full update check: closure size delta + package diff         |
| `a` / `y` / `h` | target all profiles / system (sYs) / home (home-manager)   |
| `s` / `b` / `d` | deploy: switch now / boot entry / dry run                  |
| `x`            | cancel the running deploy                                    |
| `/`            | search the focused log pane                                  |
| `n` / `N`      | next / previous search match                                 |
| `1`–`5`        | toggle deploy-rs flags (see below)                           |
| `o`            | open the SSH overrides menu for the selected host            |

### Toggles (`1`–`5`)

| key | flag                       | default | notes                                                   |
| --- | -------------------------- | ------- | ------------------------------------------------------- |
| `1` | `--skip-checks`            | off     | skip the pre-deploy `nix flake check`                   |
| `2` | `--magic-rollback false`   | on      | wait for confirmation, auto-roll-back on timeout        |
| `3` | `--auto-rollback false`    | on      | roll back if activation itself fails                    |
| `4` | `--remote-build`           | off     | build on the target host instead of locally             |
| `5` | `--interactive-sudo true`  | off     | **will hang the TUI** — child reads password from stdin |

The toggles strip at the top of the screen always shows the current
state with a green `●` for on or grey `○` for off.

### SSH overrides (`o` then sub-key)

For hosts that aren't in `~/.ssh/config`, press `o` to open the
overrides menu, then:

| sub-key | action                                                                   |
| ------- | ------------------------------------------------------------------------ |
| `h`     | set hostname / IP override                                               |
| `u`     | set ssh user                                                             |
| `k`     | set identity file path (passed as `ssh -i`)                              |
| `o`     | set extra ssh `-o` options (whitespace-separated, e.g. `Port=2222`)      |
| `c`     | clear all overrides for this host                                        |
| `Esc`   | leave the menu                                                           |

When editing a field, type into the prompt strip at the bottom of the
screen and press `Enter` to save (or `Esc` to cancel). An empty value
clears that field. Hosts with any active override show a magenta
`[ssh]` tag in the host list and a summary line in the details pane.

These overrides are session-only — they're not persisted to disk and
don't modify your flake. They feed both the status checks and the
actual `deploy` invocation, so what you see in the badges matches what
gets pushed.

## Update-check details

### Cheap tier (`u`)

Runs `nix eval --raw <flake>#deploy.nodes.<name>.profiles.<p>.path` to
get the deploy-rs activation wrapper, resolves it to the actual system
toplevel via `nix-store --query --references`, then compares that
against `readlink -f /run/current-system` (for `system`) or the
home-manager profile symlink (for `home`). Falls back to a parsed
name+version comparison when the wrapper isn't in the local store yet.
On-demand because the eval can be slow on large flakes.

### Full tier (`Shift+U`)

After a successful `u`, this measures `nix path-info --closure-size`
on both sides, then runs a metadata-only package diff by listing
`nix-store --query --requisites` locally and remotely. Version
changes, additions, and removals are surfaced per-package. When every
package name+version matches but store paths still differ (e.g. a
config file rebuild), the TUI shows a distinct "content differs"
indicator and lists the divergent paths so the user can identify what
changed.

Stale size and package data are automatically cleared when `u` is
re-run or after a successful deploy.

### Badges

| badge       | meaning                                              |
| ----------- | ---------------------------------------------------- |
| `sys:?`     | not yet checked                                      |
| `sys:✓`     | host already runs the latest build                   |
| `sys:↑`     | host is behind — deploy would change something       |
| `sys:!`     | check failed (host unreachable, eval error, …)       |
| `sys:-`     | this profile is not defined for this host            |
| `sys:⠋`     | check in flight (animated braille spinner)           |
| `sys:✓⠋`    | check in flight, prior result was up-to-date         |

## Limitations

- Online check probes TCP/22 only. Hosts that block port 22 from your
  machine will show as offline even if they are up.
- The home-update probe assumes `~/.local/state/nix/profiles/home-manager`
  or `~/.nix-profile`. Custom profile locations aren't auto-detected.
- `--interactive-sudo` (toggle `5`) is supported by deploy-rs but the
  child reads the password from stdin, which the TUI runs as
  `Stdio::null()`. The deploy will hang silently — press `x` to kill
  the child. Use passwordless sudo on the target instead.
- The TUI shells out to `deploy` for the actual push — anything else
  that requires interactive input (e.g. host-key confirmations on a
  fresh host) won't work. Use `ssh-copy-id` first or set
  `StrictHostKeyChecking=accept-new` via the override `-o` opts.
- SSH overrides are session-only. They feed `deploy` and the status
  checks but are not persisted between runs. If you want them to stick,
  add them to your `~/.ssh/config` or to `deploy.nodes.<name>` in the
  flake.
