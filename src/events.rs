//! What `gatefold run` is doing right now, for the web UI's live panel and
//! `/api/v1/events`. Runs report through [`crate::progress::Progress`] to a
//! process-wide bus; when nothing listens (a one-off CLI run), reporting does
//! nothing.
//!
//! Jobs run in parallel, each on its own thread in a *lane* (its name, or
//! `manual`): what a thread reports is tagged with its lane, and each lane
//! can be asked to stop on its own.

use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::broadcast;

/// Something that happened.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    RunStarted {
        id: Option<i64>,
        trigger: String,
        dir: String,
    },
    /// A new part of the run (`Processing`, `Artist images`, …).
    Section {
        title: String,
    },
    /// A log line; `kind` is `ok`, `info` or `error`.
    Line {
        kind: String,
        text: String,
    },
    /// How far the current phase is (throttled).
    Progress {
        label: String,
        done: u32,
        total: u32,
        eta: String,
        current: Option<String>,
    },
    /// Sleeping off a rate limit (`secs` 0: done waiting).
    Waiting {
        reason: String,
        secs: u64,
    },
    /// A file was written, replaced or marked.
    Change {
        artist: Option<String>,
        artist_dir: Option<PathBuf>,
        text: String,
        file: String,
    },
    RunFinished {
        id: Option<i64>,
        status: String,
    },
    /// Something else changed (paused, a marker set from the UI, …).
    Changed,
}

/// An event and the lane it happened in, as sent to listeners.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Message {
    pub lane: Option<String>,
    #[serde(flatten)]
    pub event: Event,
}

/// A recent log line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveLine {
    pub at: String,
    pub kind: String,
    pub text: String,
}

/// A recent change.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveChange {
    pub at: String,
    pub artist: Option<String>,
    pub artist_dir: Option<PathBuf>,
    pub text: String,
    pub file: String,
}

/// A run going on now.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LiveRun {
    pub lane: String,
    pub run_id: Option<i64>,
    pub trigger: Option<String>,
    pub dir: Option<String>,
    pub section: Option<String>,
    pub label: Option<String>,
    pub done: u32,
    pub total: u32,
    pub eta: Option<String>,
    pub current: Option<String>,
    pub waiting: Option<String>,
    pub lines: Vec<LiveLine>,
    pub stopping: bool,
}

impl LiveRun {
    pub fn percent(&self) -> u32 {
        if self.total == 0 {
            0
        } else {
            u32::try_from((u64::from(self.done) * 100 / u64::from(self.total)).min(100))
                .unwrap_or(100)
        }
    }
}

/// What's going on right now.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Live {
    /// The runs going on, by lane.
    pub runs: Vec<LiveRun>,
    /// The latest changes, from any run.
    pub changes: Vec<LiveChange>,
}

impl Live {
    pub fn running(&self) -> bool {
        !self.runs.is_empty()
    }
}

const LINES: usize = 12;
const CHANGES: usize = 10;

#[derive(Default)]
struct State {
    runs: BTreeMap<String, LiveRun>,
    changes: VecDeque<LiveChange>,
}

struct Bus {
    sender: broadcast::Sender<Message>,
    state: Mutex<State>,
    /// Each lane's stop flag, while it runs.
    lanes: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
}

static BUS: OnceLock<Bus> = OnceLock::new();

thread_local! {
    /// The lane this thread runs in, and its stop flag.
    static LANE: RefCell<Option<(String, Arc<AtomicBool>)>> = const { RefCell::new(None) };
}

/// Start listening (`gatefold run` does, once); reporting does nothing before.
pub fn enable() {
    BUS.get_or_init(|| Bus {
        sender: broadcast::channel(512).0,
        state: Mutex::default(),
        lanes: Mutex::default(),
    });
}

pub fn enabled() -> bool {
    BUS.get().is_some()
}

/// New events, from now on; `None` when the bus isn't on.
pub fn subscribe() -> Option<broadcast::Receiver<Message>> {
    BUS.get().map(|bus| bus.sender.subscribe())
}

/// What's going on right now.
pub fn live() -> Live {
    BUS.get().map_or_else(Live::default, |bus| {
        let state = bus
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Live {
            runs: state.runs.values().cloned().collect(),
            changes: state.changes.iter().cloned().collect(),
        }
    })
}

/// Report `event`, in this thread's lane.
pub fn emit(event: Event) {
    let Some(bus) = BUS.get() else {
        return;
    };
    let lane = current_lane();
    {
        let mut state = bus
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        apply(&mut state, lane.as_deref(), &event);
    }
    let _ = bus.sender.send(Message { lane, event });
}

/// While held, this thread runs in `lane` (a job's name, or `manual`).
pub struct InLane {
    lane: String,
}

impl Drop for InLane {
    fn drop(&mut self) {
        LANE.with(|current| current.borrow_mut().take());
        if let Some(bus) = BUS.get() {
            bus.lanes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.lane);
        }
    }
}

/// Run in `lane` on this thread until the returned guard drops.
pub fn enter(lane: &str) -> InLane {
    let flag = Arc::new(AtomicBool::new(false));
    LANE.with(|current| *current.borrow_mut() = Some((lane.to_string(), flag.clone())));
    if let Some(bus) = BUS.get() {
        bus.lanes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(lane.to_string(), flag);
    }
    InLane {
        lane: lane.to_string(),
    }
}

fn current_lane() -> Option<String> {
    LANE.with(|current| current.borrow().as_ref().map(|(lane, _)| lane.clone()))
}

/// Ask the run in `lane` (every lane, with `None`) to stop after its current
/// track or artist. Returns how many were asked.
pub fn request_stop(lane: Option<&str>) -> usize {
    let Some(bus) = BUS.get() else {
        return 0;
    };
    let lanes = bus
        .lanes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut asked = 0;
    for (name, flag) in lanes.iter() {
        if lane.is_none_or(|lane| lane == name) {
            flag.store(true, Ordering::SeqCst);
            asked += 1;
        }
    }
    drop(lanes);
    if asked > 0 {
        let mut state = bus
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (name, run) in &mut state.runs {
            if lane.is_none_or(|lane| lane == name) {
                run.stopping = true;
            }
        }
    }
    asked
}

/// True once this thread's run was asked to stop.
pub fn stopping() -> bool {
    LANE.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|(_, flag)| flag.load(Ordering::SeqCst))
    })
}

fn clock() -> String {
    jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%H:%M:%S")
        .to_string()
}

fn apply(state: &mut State, lane: Option<&str>, event: &Event) {
    if let Event::Change {
        artist,
        artist_dir,
        text,
        file,
    } = event
    {
        state.changes.push_front(LiveChange {
            at: clock(),
            artist: artist.clone(),
            artist_dir: artist_dir.clone(),
            text: text.clone(),
            file: file.clone(),
        });
        state.changes.truncate(CHANGES);
        return;
    }
    let Some(lane) = lane else {
        return;
    };
    if let Event::RunStarted { id, trigger, dir } = event {
        state.runs.insert(
            lane.to_string(),
            LiveRun {
                lane: lane.to_string(),
                run_id: *id,
                trigger: Some(trigger.clone()),
                dir: Some(dir.clone()),
                ..LiveRun::default()
            },
        );
        return;
    }
    if let Event::RunFinished { .. } = event {
        state.runs.remove(lane);
        return;
    }
    let Some(run) = state.runs.get_mut(lane) else {
        return;
    };
    match event {
        Event::Section { title } => run.section = Some(title.clone()),
        Event::Line { kind, text } => {
            run.lines.insert(
                0,
                LiveLine {
                    at: clock(),
                    kind: kind.clone(),
                    text: text.clone(),
                },
            );
            run.lines.truncate(LINES);
        }
        Event::Progress {
            label,
            done,
            total,
            eta,
            current,
        } => {
            run.label = Some(label.clone());
            run.done = *done;
            run.total = *total;
            run.eta = Some(eta.clone());
            run.current.clone_from(current);
        }
        Event::Waiting { reason, secs } => {
            run.waiting = (*secs > 0).then(|| format!("{reason}: waiting {secs}s"));
        }
        Event::RunStarted { .. }
        | Event::RunFinished { .. }
        | Event::Change { .. }
        | Event::Changed => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_lane_has_its_own_live_run() {
        let mut state = State::default();
        for lane in ["new-tracks", "artist-images"] {
            apply(
                &mut state,
                Some(lane),
                &Event::RunStarted {
                    id: Some(1),
                    trigger: "schedule".into(),
                    dir: "/music".into(),
                },
            );
        }
        apply(
            &mut state,
            Some("new-tracks"),
            &Event::Progress {
                label: "processing".into(),
                done: 5,
                total: 20,
                eta: "1m".into(),
                current: Some("01. Song.flac".into()),
            },
        );
        for i in 0..20 {
            apply(
                &mut state,
                Some("artist-images"),
                &Event::Line {
                    kind: "info".into(),
                    text: format!("line {i}"),
                },
            );
        }
        assert_eq!(state.runs["new-tracks"].percent(), 25);
        assert_eq!(state.runs["artist-images"].lines.len(), LINES);
        assert_eq!(
            state.runs["artist-images"].lines[0].text, "line 19",
            "newest first"
        );
        assert_eq!(state.runs["new-tracks"].lines.len(), 0);
        apply(
            &mut state,
            Some("new-tracks"),
            &Event::RunFinished {
                id: Some(1),
                status: "ok".into(),
            },
        );
        assert_eq!(state.runs.keys().collect::<Vec<_>>(), ["artist-images"]);
    }

    #[test]
    fn a_stop_reaches_only_its_lane() {
        enable();
        let other = std::thread::spawn(|| {
            let _lane = enter("test-other-lane");
            std::thread::sleep(std::time::Duration::from_millis(100));
            stopping()
        });
        let _lane = enter("test-this-lane");
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(request_stop(Some("test-this-lane")), 1);
        assert!(stopping());
        assert!(!other.join().unwrap());
    }
}
