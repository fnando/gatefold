use crate::changes::{Change, Recorder};
use crate::events::{self, Event};
use crate::net::RateLimitWaiter;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::cell::{Cell, RefCell};
use std::io::IsTerminal;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Running average of per-track wall time, used to refine the ETA as real
/// timings arrive. Falls back to `seed` before anything has been measured.
struct Estimator {
    processed: u32,
    total_secs: f64,
    seed: f64,
}

impl Estimator {
    fn new(seed: f64) -> Self {
        Self {
            processed: 0,
            total_secs: 0.0,
            seed,
        }
    }

    fn record(&mut self, secs: f64) {
        self.processed += 1;
        self.total_secs += secs;
    }

    fn avg(&self) -> f64 {
        if self.processed == 0 {
            self.seed
        } else {
            self.total_secs / f64::from(self.processed)
        }
    }
}

/// Live, sectioned terminal reporting: a progress bar plus a transient spinner
/// used while we sleep off a rate limit.
pub struct Progress {
    multi: MultiProgress,
    bar: ProgressBar,
    total: u32,
    /// What the bar counts, for log lines ("track" / "artist").
    unit: &'static str,
    estimator: RefCell<Estimator>,
    /// Where changes go (see [`Progress::change`]); `None` records nothing.
    recorder: Option<Arc<Recorder>>,
    /// The bar's label and current item, for the live events.
    prefix: String,
    current: RefCell<Option<String>>,
    /// When progress was last reported live (at most once a second).
    reported: Cell<Option<Instant>>,
}

impl Progress {
    /// `prefix` labels the bar (e.g. `processing`).
    pub fn new(prefix: &str, total: usize, seed_secs: f64) -> Self {
        // Off a TTY (e.g. cron), indicatif can't reposition the cursor to redraw the
        // bar, which garbles and duplicates the explicit log lines. Hide the live
        // rendering there so each line is emitted once; interactive runs are unchanged.
        // `gatefold run` reports to the web UI and its log, never to a bar.
        let multi = if std::io::stderr().is_terminal() && !events::enabled() {
            MultiProgress::new()
        } else {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        };
        Self::on(multi, prefix, "track", total, seed_secs)
    }

    /// A follow-up phase sharing this reporter's terminal: a fresh bar (the previous
    /// one stays finished above it) with its own total, unit, and ETA estimator.
    pub fn phase(&self, prefix: &str, unit: &'static str, total: usize, seed_secs: f64) -> Self {
        let mut phase = Self::on(self.multi.clone(), prefix, unit, total, seed_secs);
        phase.recorder.clone_from(&self.recorder);
        phase
    }

    /// Record changes through `recorder` (shared with every [`Self::phase`]).
    pub fn with_recorder(mut self, recorder: Option<Arc<Recorder>>) -> Self {
        self.recorder = recorder;
        self
    }

    /// Note a change to a file, for `gatefold changes` and the web UI.
    pub fn change(&self, change: &Change) {
        if let Some(recorder) = &self.recorder {
            recorder.record(change);
        }
        events::emit(Event::Change {
            artist: change.artist.clone(),
            artist_dir: change.artist_dir.clone(),
            text: change.describe().to_string(),
            file: crate::changes::short_file(change),
        });
    }

    /// Keep a log line: in the run's history and on the live panel.
    fn line(&self, kind: &str, text: &str) {
        if let Some(recorder) = &self.recorder {
            recorder.line(kind, text);
        }
        events::emit(Event::Line {
            kind: kind.to_string(),
            text: text.to_string(),
        });
    }

    /// Report how far the phase is, at most once a second unless `force`.
    fn report(&self, force: bool) {
        if !events::enabled() {
            return;
        }
        let now = Instant::now();
        if !force
            && self
                .reported
                .get()
                .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            return;
        }
        self.reported.set(Some(now));
        let done = self.estimator.borrow().processed;
        events::emit(Event::Progress {
            label: self.prefix.clone(),
            done,
            total: self.total,
            eta: format_duration(self.eta()),
            current: self.current.borrow().clone(),
        });
    }

    fn on(
        multi: MultiProgress,
        prefix: &str,
        unit: &'static str,
        total: usize,
        seed_secs: f64,
    ) -> Self {
        let total = u32::try_from(total).unwrap_or(u32::MAX);
        let bar = multi.add(ProgressBar::new(u64::from(total)));
        bar.set_style(
            ProgressStyle::with_template(
                "{prefix:.bold.cyan} [{bar:30.green/dim}] {pos}/{len} {wide_msg}",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        bar.set_prefix(prefix.to_string());
        Self {
            multi,
            bar,
            total,
            unit,
            estimator: RefCell::new(Estimator::new(seed_secs)),
            recorder: None,
            prefix: prefix.to_string(),
            current: RefCell::new(None),
            reported: Cell::new(None),
        }
    }

    /// Print a single unindented line (used for the run-summary headline).
    pub fn headline(&self, msg: &str) {
        self.multi.suspend(|| eprintln!("{msg}"));
        self.line("info", msg);
    }

    /// Print a bold section header above the bar.
    pub fn section(&self, title: &str) {
        self.multi
            .suspend(|| eprintln!("\n\x1b[1m== {title} ==\x1b[0m"));
        if let Some(recorder) = &self.recorder {
            recorder.line("section", title);
        }
        events::emit(Event::Section {
            title: title.to_string(),
        });
    }

    /// Print an informational line above the bar.
    pub fn info(&self, msg: impl AsRef<str>) {
        let line = msg.as_ref();
        self.multi.suspend(|| eprintln!("  {line}"));
        self.line(line_kind(line), line);
    }

    /// Set the status message shown on the bar for the current track.
    pub fn status(&self, msg: impl Into<String>) {
        let msg = msg.into();
        self.bar.set_message(msg.clone());
        *self.current.borrow_mut() = Some(msg);
        self.report(false);
    }

    /// Advance the bar by one track and fold its wall time into the estimate.
    pub fn record_track(&self, elapsed: Duration) {
        self.estimator.borrow_mut().record(elapsed.as_secs_f64());
        self.bar.inc(1);
        self.report(false);
    }

    /// Current ETA for the remaining tracks.
    fn eta(&self) -> Duration {
        let est = self.estimator.borrow();
        let remaining = self.total.saturating_sub(est.processed);
        Duration::from_secs_f64(est.avg() * f64::from(remaining))
    }

    /// Log the initial estimate before any track is processed.
    pub fn log_initial_estimate(&self) {
        self.info(format!(
            "estimated time for {} {}s: ~{} (will refine as it runs)",
            self.total,
            self.unit,
            format_duration(self.eta())
        ));
    }

    /// Log the running progress (position, average, ETA) so it visibly converges.
    /// Called at folder boundaries so it never interrupts a folder mid-way.
    pub fn log_progress(&self) {
        let (pos, avg) = {
            let est = self.estimator.borrow();
            (est.processed, est.avg())
        };
        if pos > 0 {
            self.info(format!(
                "progress {}/{} · avg {avg:.1}s/{} · ETA ~{}",
                pos,
                self.total,
                self.unit,
                format_duration(self.eta())
            ));
        }
    }

    pub fn finish(&self) {
        self.bar.finish_with_message("done");
        *self.current.borrow_mut() = None;
        self.report(true);
    }
}

impl RateLimitWaiter for Progress {
    fn wait(&self, duration: Duration, reason: &str) {
        let spinner = self.multi.add(ProgressBar::new_spinner());
        spinner.set_style(
            ProgressStyle::with_template("{spinner:.yellow} {msg}")
                .unwrap()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
        );
        events::emit(Event::Waiting {
            reason: reason.to_string(),
            secs: duration.as_secs().max(1),
        });
        let start = Instant::now();
        while start.elapsed() < duration {
            let remaining = (duration.as_secs_f64() - start.elapsed().as_secs_f64()).max(0.0);
            spinner.set_message(format!(
                "{reason}: honoring Retry-After, resuming in {remaining:.0}s"
            ));
            spinner.tick();
            sleep(Duration::from_millis(120));
        }
        spinner.finish_and_clear();
        self.multi.remove(&spinner);
        events::emit(Event::Waiting {
            reason: reason.to_string(),
            secs: 0,
        });
    }
}

/// What a log line says by its mark: `✓` ok, `✗` error, else info.
pub fn line_kind(line: &str) -> &'static str {
    match line.trim_start().chars().next() {
        Some('✓') => "ok",
        Some('✗') => "error",
        _ => "info",
    }
}

/// Render a duration as a compact `Hh Mm Ss` string.
fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}
