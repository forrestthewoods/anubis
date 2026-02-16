use crossbeam::channel::{Receiver, Sender, TryRecvError};
use crossterm::terminal;
use std::io::Write;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::job_system::{JobDisplayInfo, JobId};
use crate::logging::LogLevel;
use crate::logging::PROGRESS_SENDER;
use crate::util::format_duration;

/// Duration thresholds for color-coded display.
const DURATION_WARN: Duration = Duration::from_secs(3);
const DURATION_CRITICAL: Duration = Duration::from_secs(15);

// ANSI color codes.
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const GRAY: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

// ----------------------------------------------------------------------------
// Events sent from worker threads to the progress display
// ----------------------------------------------------------------------------
pub enum ProgressEvent {
    JobStarted {
        worker_id: usize,
        job_id: JobId,
        display: JobDisplayInfo,
    },
    JobCompleted {
        worker_id: usize,
        job_id: JobId,
        display: JobDisplayInfo,
        duration: Duration,
    },
    WorkerIdle {
        worker_id: usize,
    },
    JobFailed {
        worker_id: usize,
        job_id: JobId,
        display: JobDisplayInfo,
        error_output: String,
    },
    /// A tracing event routed through the progress display (used in Live/TUI mode
    /// so tracing output appears in the scroll region instead of corrupting the footer).
    TracingMessage {
        level: tracing::Level,
        message: String,
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
    display: JobDisplayInfo,
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
    /// - `no_tui`: if true, force plain scrolling output (--no-tui flag)
    /// - `log_level`: controls scrollback verbosity (debug/trace show full paths)
    pub fn new(num_workers: usize, is_tty: bool, no_tui: bool, log_level: LogLevel) -> Self {
        let mode = if !no_tui && is_tty {
            DisplayMode::Live
        } else {
            DisplayMode::Simple
        };

        let verbose = matches!(log_level, LogLevel::Debug | LogLevel::Trace | LogLevel::FullVerbose);

        let (event_tx, event_rx) = crossbeam::channel::unbounded::<ProgressEvent>();

        // In Live mode, route tracing through the progress display's scroll region
        // instead of letting it write directly to stdout (which would corrupt the TUI).
        if mode == DisplayMode::Live {
            if let Ok(mut guard) = PROGRESS_SENDER.lock() {
                *guard = Some(event_tx.clone());
            }
        }

        let render_thread = {
            let mode = mode;
            Some(std::thread::spawn(move || {
                render_loop(mode, num_workers, event_rx, verbose);
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
    /// Safe to call multiple times (idempotent via `render_thread.take()`).
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        if let Some(handle) = self.render_thread.take() {
            let _ = self.event_tx.send(ProgressEvent::Shutdown);
            let _ = handle.join();

            // Clear the progress sender so tracing resumes writing to stdout
            if self.mode == DisplayMode::Live {
                if let Ok(mut guard) = PROGRESS_SENDER.lock() {
                    *guard = None;
                }
            }
        }
    }
}

impl Drop for ProgressDisplay {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

// ----------------------------------------------------------------------------
// Render loop (runs on background thread)
// ----------------------------------------------------------------------------
fn render_loop(mode: DisplayMode, num_workers: usize, event_rx: Receiver<ProgressEvent>, verbose: bool) {
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
                            display,
                            ..
                        } => {
                            if worker_id < state.worker_status.len() {
                                state.worker_status[worker_id] = Some(WorkerActivity {
                                    display,
                                    started_at: Instant::now(),
                                });
                            }
                        }
                        ProgressEvent::JobCompleted {
                            worker_id,
                            display,
                            duration,
                            ..
                        } => {
                            state.completed_jobs += 1;
                            if worker_id < state.worker_status.len() {
                                accumulate_worker_time(&mut state, worker_id);
                                state.worker_status[worker_id] = None;
                            }
                            let label = if verbose { &display.detail } else { &display.short_name };
                            let short = format!("{} {}", display.verb, label);
                            let dur = format_duration(duration);
                            scroll_messages.push(format_scroll_line(&short, &dur, duration));
                        }
                        ProgressEvent::WorkerIdle { worker_id } => {
                            if worker_id < state.worker_status.len() {
                                accumulate_worker_time(&mut state, worker_id);
                                state.worker_status[worker_id] = None;
                            }
                        }
                        ProgressEvent::JobFailed {
                            worker_id,
                            display,
                            error_output,
                            ..
                        } => {
                            state.completed_jobs += 1;
                            state.failed_jobs += 1;
                            if worker_id < state.worker_status.len() {
                                accumulate_worker_time(&mut state, worker_id);
                                state.worker_status[worker_id] = None;
                            }
                            let label = if verbose { &display.detail } else { &display.short_name };
                            let short = format!("{} {}", display.verb, label);
                            scroll_messages
                                .push(format!("{RED}    Failed{RESET} {}", short));
                            if !error_output.is_empty() {
                                // Print each line of error output
                                for line in error_output.lines() {
                                    scroll_messages.push(format!("           {}", line));
                                }
                            }
                        }
                        ProgressEvent::TracingMessage { level, message } => {
                            let color = match level {
                                tracing::Level::ERROR => RED,
                                tracing::Level::WARN => YELLOW,
                                tracing::Level::INFO => GREEN,
                                tracing::Level::DEBUG => BLUE,
                                tracing::Level::TRACE => MAGENTA,
                            };
                            scroll_messages.push(format!("{color}{:>5}{RESET} {}", level, message));
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
            match mode {
                DisplayMode::Live => {
                    // Clear the footer one last time
                    clear_footer(footer_lines);
                    render_final_summary(&state, num_workers);
                }
                DisplayMode::Simple => {
                    render_final_summary(&state, num_workers);
                }
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
            "{RED}    Failed{RESET} Build failed ({} completed, {} failed) in {} ({}% efficiency)",
            state.completed_jobs - state.failed_jobs,
            state.failed_jobs,
            elapsed_str,
            efficiency
        );
    } else {
        println!(
            "{GREEN}  Finished{RESET} {} job(s) in {} ({}% efficiency)",
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

    // Move cursor up to clear previous footer, then return to column 1.
    // \x1b[nA moves up but preserves column position, so without \r the
    // cursor would land mid-line, leaving separator fragments visible.
    if *footer_lines > 0 {
        buf.push_str(&format!("\x1b[{}A\r", *footer_lines));
        buf.push_str("\x1b[J"); // Clear from cursor to end of screen
    }

    // Print scrolling messages
    for msg in scroll_messages {
        buf.push_str(msg);
        buf.push('\n');
    }

    // Separator line
    let separator: String = "━".repeat(term_width.min(60));
    buf.push_str(&format!("{GRAY}{}{RESET}\n", separator));

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
                let short = format!("{} {}", activity.display.verb, activity.display.short_name);
                let elapsed = activity.started_at.elapsed();
                let dur = format_duration(elapsed);
                let colored_dur = color_duration(elapsed, &dur);
                format!(" {GRAY}W{}:{RESET} {} {}", i, short, colored_dur)
            }
            None => {
                format!(" {GRAY}W{}: (idle){RESET}", i)
            }
        };
        let worker_line = truncate_str(&worker_line, term_width);
        buf.push_str(&worker_line);
        if i < num_workers - 1 {
            buf.push('\n');
        }
    }

    // Footer occupies 2 + num_workers rows (separator, progress, workers).
    // The last worker line has no trailing newline, so the cursor sits on its row.
    // Move distance from last worker to separator = (num_workers - 1) + 1 + 1 = num_workers + 1.
    *footer_lines = 1 + num_workers;

    // Write everything in one go
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(buf.as_bytes());
    let _ = stdout.flush();
}

fn clear_footer(footer_lines: usize) {
    if footer_lines > 0 {
        let mut stdout = std::io::stdout().lock();
        let _ = write!(stdout, "\x1b[{}A\r\x1b[J", footer_lines);
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

/// Wrap a formatted duration string with ANSI color based on elapsed time.
///  - Under DURATION_WARN:     gray (GRAY)
///  - DURATION_WARN..CRITICAL: yellow (YELLOW)
///  - DURATION_CRITICAL+:      red (RED)
fn color_duration(elapsed: Duration, formatted: &str) -> String {
    if elapsed >= DURATION_CRITICAL {
        format!("{RED}({}){RESET}", formatted)
    } else if elapsed >= DURATION_WARN {
        format!("{YELLOW}({}){RESET}", formatted)
    } else {
        format!("{GRAY}({}){RESET}", formatted)
    }
}

/// Format a scrolling completion line with colored verb prefix.
fn format_scroll_line(short_desc: &str, duration: &str, raw_duration: Duration) -> String {
    let colored_dur = color_duration(raw_duration, duration);
    // Extract the verb (first word) to color it
    if let Some(space_idx) = short_desc.find(' ') {
        let verb = &short_desc[..space_idx];
        let rest = &short_desc[space_idx..];
        format!("{GREEN}{:>10}{RESET}{} {}", verb, rest, colored_dur)
    } else {
        format!("{GREEN}{:>10}{RESET} {}", short_desc, colored_dur)
    }
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
        format!("{}…{RESET}", &s[..last_visible_byte.saturating_sub(1)])
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_color_duration_fast() {
        let result = color_duration(Duration::from_millis(500), "500ms");
        assert_eq!(result, format!("{GRAY}(500ms){RESET}"));
    }

    #[test]
    fn test_color_duration_warn() {
        let result = color_duration(Duration::from_secs(5), "5.0s");
        assert_eq!(result, format!("{YELLOW}(5.0s){RESET}"));
    }

    #[test]
    fn test_color_duration_critical() {
        let result = color_duration(Duration::from_secs(20), "20.0s");
        assert_eq!(result, format!("{RED}(20.0s){RESET}"));
    }

    #[test]
    fn test_color_duration_at_boundaries() {
        // Exactly at warn threshold => yellow
        assert_eq!(
            color_duration(Duration::from_secs(3), "3.0s"),
            format!("{YELLOW}(3.0s){RESET}")
        );
        // Exactly at critical threshold => red
        assert_eq!(
            color_duration(Duration::from_secs(15), "15.0s"),
            format!("{RED}(15.0s){RESET}")
        );
        // Just below warn threshold => gray
        assert_eq!(
            color_duration(Duration::from_millis(2999), "3.0s"),
            format!("{GRAY}(3.0s){RESET}")
        );
    }
}
