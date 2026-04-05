# idlers

A Wayland idle daemon written in Rust. It uses the `ext-idle-notify-v1` Wayland protocol to detect user inactivity and run commands after configurable timeouts.

Inspired by [Hypridle](https://github.com/hyprwm/hypridle) and supports a subset of its config syntax. Works with any Wayland compositor that implements `ext-idle-notify-v1` (Hyprland, Sway, etc.).

## Features

- Run commands after configurable idle timeouts
- Run commands when the user resumes activity
- Idle inhibitor awareness (e.g. don't lock screen during a Discord voice call)
- Hot-reload config file on changes
- Log to stderr and/or a file
- Survives terminal detachment (safe to run with `&`)

## Building

```
cargo build --release
```

The binary will be at `target/release/idlers`.

## Configuration

Config file location: `~/.config/idlers/idlers.conf`

Override with `--config <path>`.

### Example config

```
listener {
    name = dpms toggle
    timeout = 30
    on-timeout = pidof -x swaylock && hyprctl "dispatch dpms off"
    on-resume = hyprctl "dispatch dpms on"
}

listener {
    name = lock
    timeout = 300
    on-timeout = pidof -x swaylock || swaylock -f -e -c 1d2021
}
```

An arbitrary number of listeners is supported.

### Idle inhibition

By default, idlers ignores idle inhibitors — timers will fire even if an application (e.g. Discord during a voice call) is requesting that idle be inhibited. This requires compositor support for version 2 of the `ext-idle-notify-v1` protocol.

To make a listener respect idle inhibitors, add `allow-inhibit = true`:

```
listener {
    timeout = 30
    on-timeout = hyprctl dispatch dpms off
    on-resume = hyprctl dispatch dpms on
    allow-inhibit = true
}
```

This listener will not fire while an application is inhibiting idle.

### Hypridle compatibility

*idlers* can be used with an existing hypridle.conf. It adds an optional `name` parameter for `listener` blocks and supports Hypridle's `ignore_inhibit` option. Non-`listener` sections (e.g. Hypridle's `general` block) are silently ignored.

**Note:** idlers and Hypridle have opposite defaults for idle inhibition. Hypridle respects inhibitors by default and uses `ignore_inhibit = true` to override. idlers ignores inhibitors by default and uses `allow-inhibit = true` to respect them.

When `ignore_inhibit` is found anywhere in the config, idlers automatically enters **hypridle-compatible mode**: all listeners default to respecting inhibitors, and `ignore_inhibit = true` overrides this per-listener. A notice is logged when this happens.

Mixing `allow-inhibit` and `ignore_inhibit` in the same config is not allowed and will produce an error.

### Listener fields

| Field | Required | Description |
|---|---|---|
| `name` | No | Name for logging (auto-generated if omitted) |
| `timeout` | Yes | Seconds of idle time before `on-timeout` runs |
| `on-timeout` | No | Shell command to run when the timeout fires |
| `on-resume` | No | Shell command to run when the user becomes active (only if the timeout had fired) |
| `allow-inhibit` | No | If `true`, respect idle inhibitors (default: `false`) |
| `ignore_inhibit` | No | Hypridle-compatible alternative to `allow-inhibit` (see above) |

Non-`listener` sections in the config file are ignored.

## Usage

```
idlers [OPTIONS]
```

### Options

| Option | Description |
|---|---|
| `-l`, `--log-level <LEVEL>` | Log level: error, warn, info, debug, trace (default: info) |
| `-c`, `--config <PATH>` | Path to config file |
| `--log-file <PATH>` | Log to a file (in addition to stderr) |

### Examples

```sh
# Run with default config
idlers

# Run in the background
idlers &

# Debug logging with a custom config
idlers -l debug -c ~/.config/hypr/hypridle.conf

# Log to a file
idlers --log-file /tmp/idlers.log
```

## Contributing

Pull requests are welcome.

## License

MIT
