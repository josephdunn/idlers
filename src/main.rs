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
    Connection, Dispatch, QueueHandle, delegate_noop,
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
}

struct Timer {
    name: String,
    duration_secs: u64,
    on_timeout: String,
    on_resume: String,
    started: Instant,
    fired: bool,
}

impl Timer {
    fn new(name: &str, duration_secs: u64, on_timeout: &str, on_resume: &str) -> Self {
        Self {
            name: name.to_string(),
            duration_secs,
            on_timeout: on_timeout.to_string(),
            on_resume: on_resume.to_string(),
            started: Instant::now(),
            fired: false,
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

    fn add(&mut self, name: &str, duration_secs: u64, on_timeout: &str, on_resume: &str) {
        self.timers
            .push(Timer::new(name, duration_secs, on_timeout, on_resume));
    }

    fn reset_all(&mut self) {
        for timer in &mut self.timers {
            timer.reset();
        }
    }

    fn fire_expired(&mut self) {
        for timer in &mut self.timers {
            if timer.is_expired() && !timer.fired {
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

    fn resume_fired(&mut self) {
        for timer in &mut self.timers {
            if timer.fired {
                info!(timer = timer.name, "timer resuming");
                spawn_shell(&timer.on_resume);
            }
        }
    }

    /// Seconds until the next unfired timer expires, or `None` if all have fired.
    fn next_deadline_secs(&self) -> Option<u64> {
        self.timers
            .iter()
            .filter(|t| !t.fired)
            .map(|t| t.remaining_secs())
            .min()
    }
}

struct State {
    idle: bool,
    timers: Timers,
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                state.idle = true;
                state.timers.reset_all();
                debug!("user is idle");
            }
            ext_idle_notification_v1::Event::Resumed => {
                state.idle = false;
                state.timers.resume_fired();
                debug!("user is active");
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
                other => {
                    return Err(format!(
                        "line {line}: unknown key '{other}' in listener block"
                    ));
                }
            }
        }

        let secs = timeout.ok_or(format!("line {line}: listener block missing 'timeout'"))?;
        let name = name.unwrap_or_else(|| format!("listener-{idx}"));
        timers.add(&name, secs, &on_timeout, &on_resume);
        idx += 1;
    }

    if timers.timers.is_empty() {
        return Err(format!("No listeners defined in {}", path.display()));
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

    let mut state = State {
        idle: false,
        timers,
    };

    let seat: wl_seat::WlSeat = globals
        .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=9, ())
        .expect("No seat found");

    let idle_notifier: ExtIdleNotifierV1 = globals
        .bind::<ExtIdleNotifierV1, _, _>(&qh, 1..=1, ())
        .expect("Compositor does not support ext-idle-notify-v1");

    let _notification = idle_notifier.get_idle_notification(idle_timeout_ms, &seat, &qh, ());

    // Watch config file for changes
    let inotify_fd =
        inotify::init(inotify::CreateFlags::NONBLOCK).expect("Failed to create inotify");
    inotify::add_watch(
        &inotify_fd,
        &conf,
        inotify::WatchFlags::CLOSE_WRITE | inotify::WatchFlags::MODIFY,
    )
    .expect("Failed to watch config file");

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
            let timer_deadline = if state.idle {
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
            let mut fds = [
                PollFd::new(&wayland_fd, PollFlags::IN),
                PollFd::new(&inotify_fd, PollFlags::IN),
            ];
            poll(&mut fds, timeout.as_ref()).unwrap();

            let config_changed = fds[1].revents().contains(PollFlags::IN);

            // Read any available wayland events (ok to ignore WouldBlock)
            let _ = guard.read();

            // Check for config file changes (debounce: 1 second after last change)
            if config_changed {
                // Drain inotify events
                let mut buf = [0u8; 4096];
                while rustix::io::read(&inotify_fd, &mut buf).unwrap_or(0) > 0 {}
                pending_reload = true;
                last_change = Instant::now();
            }

            if pending_reload && last_change.elapsed().as_secs() >= 1 {
                pending_reload = false;
                info!("config file changed, reloading...");
                match load_config(&conf) {
                    Ok(new_timers) => {
                        print_timers(&new_timers);
                        state.timers = new_timers;
                    }
                    Err(e) => {
                        error!("config reload failed, keeping current config: {e}");
                    }
                }
            }
        }

        // Dispatch all pending events
        event_queue.dispatch_pending(&mut state).unwrap();

        if state.idle {
            state.timers.fire_expired();
        }
    }
}
