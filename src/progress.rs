use crossbeam::channel::{Receiver, Sender, TryRecvError};
use crossterm::terminal;
use std::io::Write;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::job_system::JobId;
use crate::logging::{LogLevel, SUPPRESS_CONSOLE_LOGGING};
use crate::util::format_duration;

// ----------------------------------------------------------------------------
// Events sent from worker threads to the progress display
// ----------------------------------------------------------------------------
pub enum ProgressEvent {
    JobStarted {
        worker_id: usize,
        job_id: JobId,
        description: String,
    },
    JobCompleted {
        worker_id: usize,
        job_id: JobId,
        description: String,
        duration: Duration,
    },
    WorkerIdle {
        worker_id: usize,
    },
    JobFailed {
        worker_id: usize,
        job_id: JobId,
        description: String,
        error_output: String,
    },
    /// Provide the live job counter so the render loop can poll the current total.
    SetJobCounter {
        counter: Arc<AtomicI64>,
    },
    Shutdown,
}

// ----------------------------------------------------------------------------
// Internal state
// ----------------------------------------------------------------------------
struct WorkerActivity {
    description: String,
    started_at: Instant,
}

struct ProgressState {
    total_jobs: usize,
    completed_jobs: usize,
    failed_jobs: usize,
    worker_status: Vec<Option<WorkerActivity>>,
    start_time: Instant,
    /// Per-worker accumulated active time
    worker_active_time: Vec<Duration>,
}

#[derive(Clone, Copy, PartialEq)]
enum DisplayMode {
    Live,
    Simple,
}

// ----------------------------------------------------------------------------
// ProgressDisplay
// ----------------------------------------------------------------------------
pub struct ProgressDisplay {
    event_tx: Sender<ProgressEvent>,
    render_thread: Option<std::thread::JoinHandle<()>>,
    mode: DisplayMode,
}

impl ProgressDisplay {
    /// Create a new ProgressDisplay.
    /// - `num_workers`: number of worker threads (determines footer height)
    /// - `is_tty`: whether stdout is a TTY
    /// - `log_level`: current log level (debug/trace disables live mode)
    pub fn new(num_workers: usize, is_tty: bool, log_level: LogLevel) -> Self {
        let mode = match log_level {
            LogLevel::Info | LogLevel::Warn | LogLevel::Error if is_tty => DisplayMode::Live,
            _ => DisplayMode::Simple,
        };

        let (event_tx, event_rx) = crossbeam::channel::unbounded::<ProgressEvent>();

        // Suppress console logging in Live mode so tracing output doesn't
        // interleave with our terminal rendering
        if mode == DisplayMode::Live {
            SUPPRESS_CONSOLE_LOGGING.store(true, Ordering::SeqCst);
        }

        let render_thread = {
            let mode = mode;
            Some(std::thread::spawn(move || {
                render_loop(mode, num_workers, event_rx);
            }))
        };

        ProgressDisplay {
            event_tx,
            render_thread,
            mode,
        }
    }

    /// Get a sender handle that can be cloned and given to worker threads.
    pub fn sender(&self) -> Sender<ProgressEvent> {
        self.event_tx.clone()
    }

    /// Shut down the display, joining the render thread.
    /// Clears the footer and prints a final summary line.
    pub fn shutdown(mut self) {
        let _ = self.event_tx.send(ProgressEvent::Shutdown);
        if let Some(handle) = self.render_thread.take() {
            let _ = handle.join();
        }

        // Restore console logging
        if self.mode == DisplayMode::Live {
            SUPPRESS_CONSOLE_LOGGING.store(false, Ordering::SeqCst);
        }
    }
}

impl Drop for ProgressDisplay {
    fn drop(&mut self) {
        // If shutdown wasn't called explicitly, still try to clean up
        if self.render_thread.is_some() {
            let _ = self.event_tx.send(ProgressEvent::Shutdown);
            if let Some(handle) = self.render_thread.take() {
                let _ = handle.join();
            }
            if self.mode == DisplayMode::Live {
                SUPPRESS_CONSOLE_LOGGING.store(false, Ordering::SeqCst);
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Render loop (runs on background thread)
// ----------------------------------------------------------------------------
fn render_loop(mode: DisplayMode, num_workers: usize, event_rx: Receiver<ProgressEvent>) {
    let mut state = ProgressState {
        total_jobs: 0,
        completed_jobs: 0,
        failed_jobs: 0,
        worker_status: (0..num_workers).map(|_| None).collect(),
        start_time: Instant::now(),
        worker_active_time: vec![Duration::ZERO; num_workers],
    };

    // Live job counter polled each tick for accurate total
    let mut job_counter: Option<Arc<AtomicI64>> = None;

    // Number of footer lines currently drawn (for clearing on next render)
    let mut footer_lines: usize = 0;
    // Queued scrolling messages to print above the footer
    let mut scroll_messages: Vec<String> = Vec::new();

    loop {
        // 1. Drain all pending events
        let mut got_shutdown = false;
        loop {
            match event_rx.try_recv() {
                Ok(event) => {
                    match event {
                        ProgressEvent::JobStarted {
                            worker_id,
                            description,
                            ..
                        } => {
                            if worker_id < state.worker_status.len() {
                                state.worker_status[worker_id] = Some(WorkerActivity {
                                    description: description.clone(),
                                    started_at: Instant::now(),
                                });
                            }
                        }
                        ProgressEvent::JobCompleted {
                            worker_id,
                            description,
                            duration,
                            ..
                        } => {
                            state.completed_jobs += 1;
                            if worker_id < state.worker_status.len() {
                                accumulate_worker_time(&mut state, worker_id);
                                state.worker_status[worker_id] = None;
                            }
                            let short = format_job_short(&description);
                            let dur = format_duration(duration);
                            scroll_messages.push(format_scroll_line(&short, &dur));
                        }
                        ProgressEvent::WorkerIdle { worker_id } => {
                            if worker_id < state.worker_status.len() {
                                accumulate_worker_time(&mut state, worker_id);
                                state.worker_status[worker_id] = None;
                            }
                        }
                        ProgressEvent::JobFailed {
                            worker_id,
                            description,
                            error_output,
                            ..
                        } => {
                            state.completed_jobs += 1;
                            state.failed_jobs += 1;
                            if worker_id < state.worker_status.len() {
                                accumulate_worker_time(&mut state, worker_id);
                                state.worker_status[worker_id] = None;
                            }
                            let short = format_job_short(&description);
                            scroll_messages
                                .push(format!("\x1b[31m    Failed\x1b[0m {}", short));
                            if !error_output.is_empty() {
                                // Print each line of error output
                                for line in error_output.lines() {
                                    scroll_messages.push(format!("           {}", line));
                                }
                            }
                        }
                        ProgressEvent::SetJobCounter { counter } => {
                            job_counter = Some(counter);
                        }
                        ProgressEvent::Shutdown => {
                            got_shutdown = true;
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    got_shutdown = true;
                    break;
                }
            }
        }

        // Poll the live job counter for accurate total
        if let Some(ref counter) = job_counter {
            state.total_jobs = counter.load(Ordering::SeqCst) as usize;
        }

        // 2. Render
        match mode {
            DisplayMode::Live => {
                render_live(&state, &scroll_messages, &mut footer_lines, num_workers);
            }
            DisplayMode::Simple => {
                render_simple(&scroll_messages);
            }
        }
        scroll_messages.clear();

        // 3. Exit if shutdown
        if got_shutdown {
            if mode == DisplayMode::Live {
                // Clear the footer one last time
                clear_footer(footer_lines);
                // Print final summary
                render_final_summary(&state, num_workers);
            }
            break;
        }

        // 4. Sleep before next render tick
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Accumulate the elapsed active time for a worker that is finishing a job.
fn accumulate_worker_time(state: &mut ProgressState, worker_id: usize) {
    if let Some(ref activity) = state.worker_status[worker_id] {
        state.worker_active_time[worker_id] += activity.started_at.elapsed();
    }
}

/// Print the final summary line with efficiency stats.
fn render_final_summary(state: &ProgressState, num_workers: usize) {
    let elapsed = state.start_time.elapsed();
    let elapsed_str = format_duration(elapsed);

    // Calculate overall efficiency: sum(worker_active_time) / (num_workers * total_elapsed)
    let total_active: Duration = state.worker_active_time.iter().sum();
    let total_possible = elapsed.as_secs_f64() * num_workers as f64;
    let efficiency = if total_possible > 0.0 {
        (total_active.as_secs_f64() / total_possible * 100.0) as u32
    } else {
        0
    };

    if state.failed_jobs > 0 {
        println!(
            "\x1b[31m    Failed\x1b[0m Build failed ({} completed, {} failed) in {} ({}% efficiency)",
            state.completed_jobs - state.failed_jobs,
            state.failed_jobs,
            elapsed_str,
            efficiency
        );
    } else {
        println!(
            "\x1b[32m  Finished\x1b[0m {} job(s) in {} ({}% efficiency)",
            state.completed_jobs, elapsed_str, efficiency
        );
    }
}

// ----------------------------------------------------------------------------
// Live mode rendering
// ----------------------------------------------------------------------------
fn render_live(
    state: &ProgressState,
    scroll_messages: &[String],
    footer_lines: &mut usize,
    num_workers: usize,
) {
    let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);

    // Build the entire output as a single string to minimize flicker
    let mut buf = String::new();

    // Move cursor up to clear previous footer
    if *footer_lines > 0 {
        buf.push_str(&format!("\x1b[{}A", *footer_lines));
        buf.push_str("\x1b[J"); // Clear from cursor to end of screen
    }

    // Print scrolling messages
    for msg in scroll_messages {
        buf.push_str(msg);
        buf.push('\n');
    }

    // Separator line
    let separator: String = "━".repeat(term_width.min(60));
    buf.push_str(&format!("\x1b[2m{}\x1b[0m\n", separator));

    // Progress bar line
    let elapsed = format_duration(state.start_time.elapsed());
    let active_count = state.worker_status.iter().filter(|w| w.is_some()).count();
    let bar_width = 20;
    let progress_bar = render_progress_bar(state.completed_jobs, state.total_jobs, bar_width);

    let progress_line = format!(
        " {} {}/{} | {} active | {} elapsed",
        progress_bar, state.completed_jobs, state.total_jobs, active_count, elapsed
    );
    // Truncate to terminal width
    let progress_line = truncate_str(&progress_line, term_width);
    buf.push_str(&progress_line);
    buf.push('\n');

    // Per-worker status lines
    for (i, worker) in state.worker_status.iter().enumerate() {
        let worker_line = match worker {
            Some(activity) => {
                let short = format_job_short(&activity.description);
                let dur = format_duration(activity.started_at.elapsed());
                format!(" \x1b[2mW{}:\x1b[0m {} \x1b[2m({})\x1b[0m", i, short, dur)
            }
            None => {
                format!(" \x1b[2mW{}: (idle)\x1b[0m", i)
            }
        };
        let worker_line = truncate_str(&worker_line, term_width);
        buf.push_str(&worker_line);
        if i < num_workers - 1 {
            buf.push('\n');
        }
    }

    // Footer occupies: 1 (separator) + 1 (progress) + num_workers (worker lines).
    // The last worker line has no trailing newline, so the cursor sits on its row.
    // To move back up to the separator row: num_workers (workers) + 1 (progress) = num_workers + 1.
    *footer_lines = 1 + num_workers;

    // Write everything in one go
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(buf.as_bytes());
    let _ = stdout.flush();
}

fn clear_footer(footer_lines: usize) {
    if footer_lines > 0 {
        let mut stdout = std::io::stdout().lock();
        let _ = write!(stdout, "\x1b[{}A\x1b[J", footer_lines);
        let _ = stdout.flush();
    }
}

fn render_progress_bar(completed: usize, total: usize, width: usize) -> String {
    if total == 0 {
        return format!("[{}]", "░".repeat(width));
    }
    let pct = (completed as f64 / total as f64).min(1.0);
    let filled = (pct * width as f64) as usize;
    let empty = width - filled;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

// ----------------------------------------------------------------------------
// Simple mode rendering (non-TTY / debug log levels)
// ----------------------------------------------------------------------------
fn render_simple(scroll_messages: &[String]) {
    if scroll_messages.is_empty() {
        return;
    }
    let mut stdout = std::io::stdout().lock();
    for msg in scroll_messages {
        let _ = writeln!(stdout, "{}", msg);
    }
    let _ = stdout.flush();
}

// ----------------------------------------------------------------------------
// Job description formatting
// ----------------------------------------------------------------------------

/// Format a scrolling completion line with colored verb prefix.
fn format_scroll_line(short_desc: &str, duration: &str) -> String {
    // Extract the verb (first word) to color it
    if let Some(space_idx) = short_desc.find(' ') {
        let verb = &short_desc[..space_idx];
        let rest = &short_desc[space_idx..];
        format!("\x1b[32m{:>10}\x1b[0m{} \x1b[2m({})\x1b[0m", verb, rest, duration)
    } else {
        format!("\x1b[32m{:>10}\x1b[0m \x1b[2m({})\x1b[0m", short_desc, duration)
    }
}

/// Convert a verbose job description into a short display string.
///
/// Known patterns:
///   "Compile cpp file [/path/to/foo.cpp]"  -> "Compiling foo.cpp"
///   "Compile c file [/path/to/foo.c]"      -> "Compiling foo.c"
///   "Build CcBinary Target //p:n with mode m (link)"  -> "Linking n"
///   "Build CcStaticLibrary Target //p:n with mode m (create archive)" -> "Archiving n"
///   "Build CcBinary Target //p:n with mode m" -> "Building n"
///   "Build ZigGlibc: target_triple"        -> "Building ZigGlibc target_triple"
///   "nasm ["/path/to/file.asm"]"           -> "Assembling file.asm"
fn format_job_short(desc: &str) -> String {
    // Compile file pattern: "Compile {lang} file [{path}]"
    if desc.starts_with("Compile ") {
        if let Some(bracket_start) = desc.rfind('[') {
            if let Some(bracket_end) = desc.rfind(']') {
                let path = &desc[bracket_start + 1..bracket_end];
                let filename = path.rsplit(['/', '\\']).next().unwrap_or(path);
                return format!("Compiling {}", filename);
            }
        }
    }

    // Link continuation: contains "(link)"
    if desc.ends_with("(link)") {
        if let Some(name) = extract_target_name(desc) {
            return format!("Linking {}", name);
        }
    }

    // Archive continuation: contains "(create archive)"
    if desc.ends_with("(create archive)") {
        if let Some(name) = extract_target_name(desc) {
            return format!("Archiving {}", name);
        }
    }

    // NASM pattern: "nasm ["/path/to/file.asm"]"
    if desc.starts_with("nasm [") {
        if let Some(bracket_start) = desc.find('[') {
            if let Some(bracket_end) = desc.rfind(']') {
                let path = desc[bracket_start + 1..bracket_end].trim_matches('"');
                let filename = path.rsplit(['/', '\\']).next().unwrap_or(path);
                return format!("Assembling {}", filename);
            }
        }
    }

    // ZigGlibc pattern: "Build ZigGlibc: target"
    if desc.starts_with("Build ZigGlibc:") {
        let target = desc.trim_start_matches("Build ZigGlibc:").trim();
        return format!("Building ZigGlibc {}", target);
    }

    // Generic build pattern: "Build XxxYyy Target //path:name with mode ..."
    if desc.starts_with("Build ") {
        if let Some(name) = extract_target_name(desc) {
            return format!("Building {}", name);
        }
    }

    // AnubisCmd pattern
    if desc.starts_with("Finalize AnubisCmd") {
        if let Some(target_start) = desc.find("//") {
            let target = &desc[target_start..];
            let name = target.rsplit(':').next().unwrap_or(target);
            return format!("Running {}", name);
        }
    }

    // Aggregate continuation
    if desc.ends_with("(aggregate)") {
        if let Some(name) = extract_target_name(desc) {
            return format!("Aggregating {}", name);
        }
    }

    // Fallback: use the description as-is, truncated
    desc.to_string()
}

/// Extract target name from a description containing "//path:name".
fn extract_target_name(desc: &str) -> Option<String> {
    // Look for "//...:" pattern
    if let Some(double_slash) = desc.find("//") {
        let after = &desc[double_slash..];
        if let Some(colon) = after.find(':') {
            // Extract name after colon, up to next space or end
            let name_start = colon + 1;
            let rest = &after[name_start..];
            let name = rest.split_whitespace().next().unwrap_or(rest);
            return Some(name.to_string());
        }
    }
    None
}

/// Truncate a string to fit within `max_width`, accounting for ANSI escape codes.
fn truncate_str(s: &str, max_width: usize) -> String {
    // Count visible characters (skip ANSI escape sequences)
    let mut visible_len = 0;
    let mut in_escape = false;
    let mut last_visible_byte = 0;

    for (i, c) in s.char_indices() {
        if c == '\x1b' {
            in_escape = true;
            continue;
        }
        if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
            continue;
        }
        visible_len += 1;
        if visible_len <= max_width {
            last_visible_byte = i + c.len_utf8();
        }
    }

    if visible_len <= max_width {
        s.to_string()
    } else {
        // Truncate and reset any open ANSI sequences
        format!("{}…\x1b[0m", &s[..last_visible_byte.saturating_sub(1)])
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_compile_cpp() {
        assert_eq!(
            format_job_short("Compile cpp file [/home/user/project/src/main.cpp]"),
            "Compiling main.cpp"
        );
    }

    #[test]
    fn test_format_compile_c() {
        assert_eq!(
            format_job_short("Compile c file [/home/user/project/src/util.c]"),
            "Compiling util.c"
        );
    }

    #[test]
    fn test_format_link() {
        assert_eq!(
            format_job_short("Build CcBinary Target //samples/basic/simple_cpp:simple_cpp with mode win_dev (link)"),
            "Linking simple_cpp"
        );
    }

    #[test]
    fn test_format_archive() {
        assert_eq!(
            format_job_short("Build CcStaticLibrary Target //libs/mylib:mylib with mode win_dev (create archive)"),
            "Archiving mylib"
        );
    }

    #[test]
    fn test_format_build_target() {
        assert_eq!(
            format_job_short("Build CcBinary Target //samples/basic/simple_cpp:simple_cpp with mode win_dev"),
            "Building simple_cpp"
        );
    }

    #[test]
    fn test_format_zig() {
        assert_eq!(
            format_job_short("Build ZigGlibc: x86_64-linux-gnu"),
            "Building ZigGlibc x86_64-linux-gnu"
        );
    }

    #[test]
    fn test_format_nasm() {
        assert_eq!(
            format_job_short("nasm [\"/home/user/project/src/boot.asm\"]"),
            "Assembling boot.asm"
        );
    }

    #[test]
    fn test_format_fallback() {
        assert_eq!(
            format_job_short("some unknown description"),
            "some unknown description"
        );
    }

    #[test]
    fn test_progress_bar_zero() {
        assert_eq!(render_progress_bar(0, 0, 10), "[░░░░░░░░░░]");
    }

    #[test]
    fn test_progress_bar_half() {
        assert_eq!(render_progress_bar(5, 10, 10), "[█████░░░░░]");
    }

    #[test]
    fn test_progress_bar_full() {
        assert_eq!(render_progress_bar(10, 10, 10), "[██████████]");
    }

    #[test]
    fn test_extract_target_name() {
        assert_eq!(
            extract_target_name("Build CcBinary Target //path/to:my_target with mode win_dev"),
            Some("my_target".to_string())
        );
    }
}
