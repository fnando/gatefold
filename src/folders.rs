//! Folders being worked on. `gatefold run` runs jobs in parallel; a run
//! takes an album folder (for its tracks' tags and sidecars) or the artist
//! folders it writes into, so two runs never write the same files at once.
//! A one-off run is alone in its process, so these never wait there.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

static BUSY: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
static FREED: Condvar = Condvar::new();

/// Folders held until dropped.
#[derive(Debug)]
pub struct Held {
    folders: Vec<PathBuf>,
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut busy = BUSY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(busy) = busy.as_mut() {
            for folder in &self.folders {
                busy.remove(folder);
            }
        }
        FREED.notify_all();
    }
}

/// Wait until none of `folders` is held by another run, then hold them all.
/// Taking them together (never one by one while holding some) means two runs
/// can't each wait for the other.
pub fn hold<'a>(folders: impl IntoIterator<Item = &'a Path>) -> Held {
    let mut wanted: Vec<PathBuf> = folders.into_iter().map(Path::to_path_buf).collect();
    wanted.sort();
    wanted.dedup();
    let mut busy = BUSY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        let set = busy.get_or_insert_with(HashSet::new);
        if wanted.iter().all(|folder| !set.contains(folder)) {
            set.extend(wanted.iter().cloned());
            return Held { folders: wanted };
        }
        busy = FREED
            .wait(busy)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn a_held_folder_waits_for_its_holder() {
        let album = Path::new("/gatefold-test/folders/album");
        let held = hold([album]);
        let got_it = Arc::new(AtomicBool::new(false));
        let waiter = {
            let got_it = got_it.clone();
            std::thread::spawn(move || {
                let _held = hold([Path::new("/gatefold-test/folders/album")]);
                got_it.store(true, Ordering::SeqCst);
            })
        };
        // Another folder is free meanwhile.
        drop(hold([Path::new("/gatefold-test/folders/other")]));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!got_it.load(Ordering::SeqCst), "still held");
        drop(held);
        waiter.join().unwrap();
        assert!(got_it.load(Ordering::SeqCst));
    }
}
