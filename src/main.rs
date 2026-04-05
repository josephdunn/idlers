use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use clap::Parser;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::inotify;
use tracing::{debug, error, info};
use tracing_subscriber::prelude::*;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

#[derive(Parser)]
#[command(name = "idlers", about = "Wayland idle event daemon")]
struct Args {
    /// Log level (error, warn, info, debug, trace)
    #[arg(short, long, default_value = "info")]
    log_level: tracing::Level,

    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Log to a file (in addition to stderr)
    #[arg(long)]
    log_file: Option<PathBuf>,

    /// Disable hot-reloading of the config file
    #[arg(long)]
    no_reload: bool,
}

#[derive(Clone, Copy)]
enum IdleSource {
    Input,
    Inhibit,
}

struct Timer {
    name: String,
    duration_secs: u64,
    on_timeout: String,
    on_resume: String,
    allow_inhibit: bool,
    started: Instant,
    fired: bool,
    active: bool,
}

impl Timer {
    fn new(
        name: &str,
        duration_secs: u64,
        on_timeout: &str,
        on_resume: &str,
        allow_inhibit: bool,
    ) -> Self {
        Self {
            name: name.to_string(),
            duration_secs,
            on_timeout: on_timeout.to_string(),
            on_resume: on_resume.to_string(),
            allow_inhibit,
            started: Instant::now(),
            fired: false,
            active: false,
        }
    }

    fn reset(&mut self) {
        self.started = Instant::now();
        self.fired = false;
    }

    fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn remaining_secs(&self) -> u64 {
        self.duration_secs.saturating_sub(self.elapsed_secs())
    }

    fn is_expired(&self) -> bool {
        self.elapsed_secs() >= self.duration_secs
    }
}

struct Timers {
    timers: Vec<Timer>,
}

impl Timers {
    fn new() -> Self {
        Self { timers: Vec::new() }
    }

    fn add(
        &mut self,
        name: &str,
        duration_secs: u64,
        on_timeout: &str,
        on_resume: &str,
        allow_inhibit: bool,
    ) {
        self.timers.push(Timer::new(
            name,
            duration_secs,
            on_timeout,
            on_resume,
            allow_inhibit,
        ));
    }

    fn update_idle_state(&mut self, input_idle: bool, inhibit_idle: bool) {
        for timer in &mut self.timers {
            let should_be_active = if timer.allow_inhibit {
                input_idle && inhibit_idle
            } else {
                input_idle
            };

            if !timer.active && should_be_active {
                // Becoming active: reset and start counting
                timer.reset();
                timer.active = true;
            } else if timer.active && !should_be_active {
                // Becoming inactive: fire on_resume if timer had fired
                if timer.fired {
                    info!(timer = timer.name, "timer resuming");
                    spawn_shell(&timer.on_resume);
                }
                timer.active = false;
            }
        }
    }

    fn fire_expired(&mut self) {
        for timer in &mut self.timers {
            if timer.active && timer.is_expired() && !timer.fired {
                timer.fired = true;
                info!(
                    timer = timer.name,
                    duration_secs = timer.duration_secs,
                    "timer fired"
                );
                spawn_shell(&timer.on_timeout);
            }
        }
    }

    fn any_active(&self) -> bool {
        self.timers.iter().any(|t| t.active)
    }

    /// Seconds until the next unfired active timer expires, or `None` if none.
    fn next_deadline_secs(&self) -> Option<u64> {
        self.timers
            .iter()
            .filter(|t| t.active && !t.fired)
            .map(|t| t.remaining_secs())
            .min()
    }
}

struct State {
    input_idle: bool,
    inhibit_idle: bool,
    has_v2: bool,
    timers: Timers,
}

impl State {
    fn update_timers(&mut self) {
        self.timers.update_idle_state(self.input_idle, self.inhibit_idle);
    }
}

impl Dispatch<ExtIdleNotificationV1, IdleSource> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        data: &IdleSource,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                match data {
                    IdleSource::Input => {
                        state.input_idle = true;
                        debug!("user is idle (input)");
                    }
                    IdleSource::Inhibit => {
                        state.inhibit_idle = true;
                        if !state.has_v2 {
                            state.input_idle = true;
                        }
                        debug!("user is idle (inhibit-aware)");
                    }
                }
                state.update_timers();
            }
            ext_idle_notification_v1::Event::Resumed => {
                match data {
                    IdleSource::Input => {
                        state.input_idle = false;
                        debug!("user is active (input)");
                    }
                    IdleSource::Inhibit => {
                        state.inhibit_idle = false;
                        if !state.has_v2 {
                            state.input_idle = false;
                        }
                        debug!("user is active (inhibit-aware)");
                    }
                }
                state.update_timers();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ExtIdleNotifierV1);

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    let dir = PathBuf::from(home).join(".config/idlers");
    fs::create_dir_all(&dir).expect("Failed to create config directory");
    dir.join("idlers.conf")
}

fn load_config(path: &PathBuf) -> Result<Timers, String> {
    let content =
        fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;

    let mut timers = Timers::new();
    let mut idx = 0;
    let mut line = 1usize;
    let mut has_allow_inhibit = false;
    let mut has_ignore_inhibit = false;
    let mut allow_inhibit_flags: Vec<Option<bool>> = Vec::new();
    let mut ignore_inhibit_flags: Vec<Option<bool>> = Vec::new();

    let mut chars = content.chars().peekable();
    while chars.peek().is_some() {
        skip_whitespace(&mut chars, &mut line);
        if chars.peek().is_none() {
            break;
        }

        // Read section name
        let word: String = chars.by_ref().take_while(|c| c.is_alphanumeric()).collect();

        skip_whitespace(&mut chars, &mut line);

        // Expect '{'
        match chars.next() {
            Some('{') => {}
            other => {
                return Err(format!(
                    "line {line}: expected '{{' after '{word}', got {other:?}"
                ));
            }
        }

        // Skip non-listener sections
        if word != "listener" {
            for c in chars.by_ref() {
                if c == '\n' {
                    line += 1;
                }
                if c == '}' {
                    break;
                }
            }
            continue;
        }

        let mut name: Option<String> = None;
        let mut timeout: Option<u64> = None;
        let mut on_timeout = String::new();
        let mut on_resume = String::new();
        let mut allow_inhibit: Option<bool> = None;
        let mut ignore_inhibit: Option<bool> = None;

        // Parse key = value lines until '}'
        loop {
            skip_whitespace(&mut chars, &mut line);
            match chars.peek() {
                Some('}') => {
                    chars.next();
                    break;
                }
                None => {
                    return Err(format!(
                        "line {line}: unexpected end of file inside listener block"
                    ));
                }
                _ => {}
            }

            let key: String = chars
                .by_ref()
                .take_while(|c| *c != '=')
                .collect::<String>()
                .trim()
                .to_string();

            let value: String = chars
                .by_ref()
                .take_while(|c| *c != '\n')
                .collect::<String>()
                .trim()
                .to_string();
            line += 1;

            match key.as_str() {
                "name" => name = Some(value),
                "timeout" => {
                    timeout =
                        Some(value.parse().map_err(|_| {
                            format!("line {line}: invalid timeout value: '{value}'")
                        })?);
                }
                "on-timeout" => on_timeout = value,
                "on-resume" => on_resume = value,
                "allow-inhibit" => {
                    allow_inhibit = Some(match value.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => {
                            return Err(format!(
                                "line {line}: invalid allow-inhibit value: '{value}' (expected true or false)"
                            ));
                        }
                    });
                }
                "ignore_inhibit" => {
                    ignore_inhibit = Some(match value.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => {
                            return Err(format!(
                                "line {line}: invalid ignore_inhibit value: '{value}' (expected true or false)"
                            ));
                        }
                    });
                }
                other => {
                    return Err(format!(
                        "line {line}: unknown key '{other}' in listener block"
                    ));
                }
            }
        }

        let secs = timeout.ok_or(format!("line {line}: listener block missing 'timeout'"))?;
        let name = name.unwrap_or_else(|| format!("listener-{idx}"));
        // Store raw flags; we resolve the default after parsing all blocks
        timers.add(&name, secs, &on_timeout, &on_resume, allow_inhibit.unwrap_or(false));
        // Track per-block flags for cross-block validation
        if let Some(v) = allow_inhibit {
            if v {
                has_allow_inhibit = true;
            }
        }
        if let Some(v) = ignore_inhibit {
            has_ignore_inhibit = true;
            ignore_inhibit_flags.push(Some(v));
        } else {
            ignore_inhibit_flags.push(None);
        }
        allow_inhibit_flags.push(allow_inhibit);
        idx += 1;
    }

    if timers.timers.is_empty() {
        return Err(format!("No listeners defined in {}", path.display()));
    }

    // Validate: allow-inhibit and ignore_inhibit cannot coexist
    if has_allow_inhibit && has_ignore_inhibit {
        return Err(
            "conflicting config: allow-inhibit and ignore_inhibit cannot both be used".to_string(),
        );
    }

    // Hypridle compat: if ignore_inhibit appears anywhere, default to allowing inhibition
    if has_ignore_inhibit {
        info!("hypridle-compatible mode: ignore_inhibit found in config, defaulting to allow-inhibit=true");

        for (i, timer) in timers.timers.iter_mut().enumerate() {
            if let Some(true) = ignore_inhibit_flags[i] {
                timer.allow_inhibit = false;
            } else if allow_inhibit_flags[i].is_none() {
                timer.allow_inhibit = true;
            }
        }
    }

    Ok(timers)
}

fn print_timers(timers: &Timers) {
    info!("loaded {} listener(s):", timers.timers.len());
    for t in &timers.timers {
        let mut desc = format!("  {} ({}s)", t.name, t.duration_secs);
        if !t.on_timeout.is_empty() {
            desc.push_str(&format!(" on-timeout=\"{}\"", t.on_timeout));
        }
        if !t.on_resume.is_empty() {
            desc.push_str(&format!(" on-resume=\"{}\"", t.on_resume));
        }
        if t.allow_inhibit {
            desc.push_str(" allow-inhibit");
        }
        info!("{desc}");
    }
}

fn skip_whitespace(chars: &mut std::iter::Peekable<std::str::Chars>, line: &mut usize) {
    while let Some(c) = chars.peek() {
        if *c == '#' {
            // Skip until end of line
            for c in chars.by_ref() {
                if c == '\n' {
                    *line += 1;
                    break;
                }
            }
        } else if *c == '\n' {
            *line += 1;
            chars.next();
        } else if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
}

fn spawn_shell(cmd: &str) {
    if cmd.is_empty() {
        return;
    }
    let cmd = cmd.to_string();
    std::thread::spawn(move || {
        let result = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        match result {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                for line in stdout.lines() {
                    info!(cmd = %cmd, "stdout: {line}");
                }
                for line in stderr.lines() {
                    info!(cmd = %cmd, "stderr: {line}");
                }
                if !output.status.success() {
                    let code = output
                        .status
                        .code()
                        .map_or("signal".to_string(), |c| c.to_string());
                    error!(cmd = %cmd, code = %code, "command failed");
                }
            }
            Err(e) => {
                error!(cmd = %cmd, "failed to run command: {e}");
            }
        }
    });
}

/// A writer that silently ignores write errors (e.g. broken pipe after TTY closes)
struct SafeStderr;

impl std::io::Write for SafeStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match std::io::stderr().write(buf) {
            Ok(n) => Ok(n),
            Err(_) => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stderr().flush();
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SafeStderr {
    type Writer = SafeStderr;

    fn make_writer(&'a self) -> Self::Writer {
        SafeStderr
    }
}

fn main() {
    // Ignore SIGPIPE so the process isn't killed if TTY closes
    {
        unsafe extern "C" {
            safe fn signal(sig: i32, handler: usize) -> usize;
        }
        signal(13, 1); // 13 = SIGPIPE, 1 = SIG_IGN
    }

    let args = Args::parse();

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(SafeStderr);

    let file_layer = args.log_file.map(|path| {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap_or_else(|e| {
                eprintln!("Failed to open log file {}: {e}", path.display());
                std::process::exit(1);
            });
        tracing_subscriber::fmt::layer()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::from_level(
            args.log_level,
        ))
        .with(stderr_layer)
        .with(file_layer)
        .init();

    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland display");
    let (globals, mut event_queue) = registry_queue_init::<State>(&conn).unwrap();
    let qh = event_queue.handle();

    let conf = args.config.unwrap_or_else(config_path);
    let timers = match load_config(&conf) {
        Ok(t) => t,
        Err(e) => {
            error!("{e}");
            std::process::exit(1);
        }
    };
    print_timers(&timers);

    // Detect idle after 1 second of inactivity; listener timeouts count from there
    let idle_timeout_ms: u32 = 1_000;

    let seat: wl_seat::WlSeat = globals
        .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=9, ())
        .expect("No seat found");

    // Try v2 first (supports input-only idle detection), fall back to v1
    let idle_notifier: ExtIdleNotifierV1 = globals
        .bind::<ExtIdleNotifierV1, _, _>(&qh, 2..=2, ())
        .or_else(|_| globals.bind::<ExtIdleNotifierV1, _, _>(&qh, 1..=1, ()))
        .expect("Compositor does not support ext-idle-notify-v1");

    let has_v2 = idle_notifier.version() >= 2;

    // Input-based notification (ignores inhibitors) — only available with v2
    let _input_notification = if has_v2 {
        info!("using input-based idle detection (ignores idle inhibitors)");
        Some(idle_notifier.get_input_idle_notification(
            idle_timeout_ms,
            &seat,
            &qh,
            IdleSource::Input,
        ))
    } else {
        info!("v2 not available; allow-inhibit has no effect");
        None
    };

    // Inhibit-aware notification (respects inhibitors) — always created
    let _inhibit_notification =
        idle_notifier.get_idle_notification(idle_timeout_ms, &seat, &qh, IdleSource::Inhibit);

    let mut state = State {
        input_idle: false,
        inhibit_idle: false,
        has_v2,
        timers,
    };

    // Watch config directory for changes (not the file directly, since editors
    // like Neovim do atomic saves that replace the inode)
    let inotify_fd = if !args.no_reload {
        let config_filename_owned = conf
            .file_name()
            .expect("Config path has no filename")
            .to_os_string();
        let fd =
            inotify::init(inotify::CreateFlags::NONBLOCK).expect("Failed to create inotify");
        inotify::add_watch(
            &fd,
            conf.parent().unwrap(),
            inotify::WatchFlags::CLOSE_WRITE
                | inotify::WatchFlags::MODIFY
                | inotify::WatchFlags::CREATE
                | inotify::WatchFlags::MOVED_TO,
        )
        .expect("Failed to watch config directory");
        Some((fd, config_filename_owned))
    } else {
        None
    };

    let mut last_change = Instant::now();
    let mut pending_reload = false;

    info!("listening for idle events...");

    loop {
        // Flush outgoing requests
        conn.flush().unwrap();

        // Try to prepare a read — returns None if events are already queued
        if let Some(guard) = event_queue.prepare_read() {
            // Compute poll timeout: shortest of timer deadline (if idle) and
            // pending reload debounce
            let timer_deadline = if state.timers.any_active() {
                state.timers.next_deadline_secs()
            } else {
                None
            };

            let debounce_deadline = if pending_reload {
                Some(1u64.saturating_sub(last_change.elapsed().as_secs()))
            } else {
                None
            };

            let timeout = timer_deadline
                .into_iter()
                .chain(debounce_deadline)
                .min()
                .map(|s| Timespec {
                    tv_sec: s as i64,
                    tv_nsec: 0,
                });

            let wayland_fd = guard.connection_fd();

            if let Some((ref inotify, ref config_filename)) = inotify_fd {
                let mut fds = [
                    PollFd::new(&wayland_fd, PollFlags::IN),
                    PollFd::new(inotify, PollFlags::IN),
                ];
                poll(&mut fds, timeout.as_ref()).unwrap();

                let config_changed = fds[1].revents().contains(PollFlags::IN);

                // Read any available wayland events (ok to ignore WouldBlock)
                let _ = guard.read();

                // Check for config file changes (debounce: 1 second after last change)
                if config_changed {
                    let mut buf = [std::mem::MaybeUninit::uninit(); 4096];
                    let mut reader = inotify::Reader::new(inotify, &mut buf);
                    loop {
                        match reader.next() {
                            Ok(event) => {
                                if let Some(name) = event.file_name() {
                                    if name.to_bytes() == config_filename.as_encoded_bytes() {
                                        pending_reload = true;
                                        last_change = Instant::now();
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            } else {
                let mut fds = [PollFd::new(&wayland_fd, PollFlags::IN)];
                poll(&mut fds, timeout.as_ref()).unwrap();
                let _ = guard.read();
            }

            if pending_reload && last_change.elapsed().as_secs() >= 1 {
                pending_reload = false;
                info!("config file changed, reloading...");
                match load_config(&conf) {
                    Ok(new_timers) => {
                        print_timers(&new_timers);
                        state.timers = new_timers;
                        state.update_timers();
                    }
                    Err(e) => {
                        error!("config reload failed, keeping current config: {e}");
                    }
                }
            }
        }

        // Dispatch all pending events
        event_queue.dispatch_pending(&mut state).unwrap();

        if state.timers.any_active() {
            state.timers.fire_expired();
        }
    }
}
