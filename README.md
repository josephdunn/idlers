# idlers

A Wayland idle daemon written in Rust. It uses the `ext-idle-notify-v1` Wayland protocol to detect user inactivity and run commands after configurable timeouts.

Inspired by [Hypridle](https://github.com/hyprwm/hypridle) and supports a subset of its config syntax. Works with any Wayland compositor that implements `ext-idle-notify-v1` (Hyprland, Sway, etc.).

## Features

- Run commands after configurable idle timeouts
- Run commands when the user resumes activity
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

### Hypridle compatibility

*idlers* can be used with an existing hypridle.conf. It adds an optional `name` parameter for `listener` blocks and does not support Hypridle's `ignore_inhibit` option. Additionally, it does not support Hypridle's `general` block, but will silently ignore it.

### Listener fields

| Field | Required | Description |
|---|---|---|
| `name` | No | Name for logging (auto-generated if omitted) |
| `timeout` | Yes | Seconds of idle time before `on-timeout` runs |
| `on-timeout` | No | Shell command to run when the timeout fires |
| `on-resume` | No | Shell command to run when the user becomes active (only if the timeout had fired) |

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
