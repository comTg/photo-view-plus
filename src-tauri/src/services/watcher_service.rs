use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use tauri::AppHandle;

use crate::config::AppPaths;
use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::queue::Scheduler;
use crate::repo::roots_repo::{self, Root};
use crate::services::scan_service::{self, ScanRootTask};

const WATCH_DEBOUNCE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatcherStatus {
    pub running: bool,
    pub watched_roots: Vec<i64>,
    pub skipped_network_roots: Vec<i64>,
    pub last_error: Option<String>,
}

pub struct WatcherService {
    pool: Pool,
    scheduler: Scheduler,
    app: AppHandle<tauri::Wry>,
    paths: AppPaths,
    state: Mutex<WatcherState>,
}

struct WatcherState {
    watcher: Option<RecommendedWatcher>,
    watched_roots: Vec<i64>,
    skipped_network_roots: Vec<i64>,
    last_error: Option<String>,
}

impl WatcherService {
    pub fn new(
        pool: Pool,
        scheduler: Scheduler,
        app: AppHandle<tauri::Wry>,
        paths: AppPaths,
    ) -> Self {
        Self {
            pool,
            scheduler,
            app,
            paths,
            state: Mutex::new(WatcherState {
                watcher: None,
                watched_roots: Vec::new(),
                skipped_network_roots: Vec::new(),
                last_error: None,
            }),
        }
    }

    pub fn start(&self) -> AppResult<WatcherStatus> {
        self.stop()?;
        let roots = {
            let conn = self.pool.get()?;
            roots_repo::list(&conn)?
                .into_iter()
                .filter(|root| root.enabled)
                .collect::<Vec<_>>()
        };
        let local_roots = roots
            .iter()
            .filter(|root| root.root_type != "network")
            .cloned()
            .collect::<Vec<_>>();
        let skipped_network_roots = roots
            .iter()
            .filter(|root| root.root_type == "network")
            .map(|root| root.id)
            .collect::<Vec<_>>();

        let pool = self.pool.clone();
        let scheduler = self.scheduler.clone();
        let app = self.app.clone();
        let paths = self.paths.clone();
        let roots_for_callback = local_roots.clone();
        let debounce = Arc::new(Mutex::new(HashMap::<i64, Instant>::new()));
        let callback_debounce = debounce.clone();
        let mut watcher = notify::recommended_watcher(move |event| {
            if let Err(error) = handle_event(
                event,
                &roots_for_callback,
                &pool,
                &scheduler,
                &app,
                &paths,
                &callback_debounce,
            ) {
                tracing::warn!(%error, "watcher event handling failed");
            }
        })
        .map_err(|error| AppError::Other(format!("watcher: {error}")))?;

        let mut watched_roots = Vec::new();
        let mut last_error = None;
        for root in &local_roots {
            let path = PathBuf::from(&root.path);
            match watcher.watch(&path, RecursiveMode::Recursive) {
                Ok(()) => watched_roots.push(root.id),
                Err(error) => {
                    last_error = Some(format!("{}: {error}", path.display()));
                    tracing::warn!(path = ?path, %error, "failed to watch root");
                }
            }
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| AppError::Other("watcher state poisoned".to_string()))?;
        state.watcher = Some(watcher);
        state.watched_roots = watched_roots;
        state.skipped_network_roots = skipped_network_roots;
        state.last_error = last_error;
        Ok(state.status())
    }

    pub fn stop(&self) -> AppResult<WatcherStatus> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AppError::Other("watcher state poisoned".to_string()))?;
        state.watcher = None;
        state.watched_roots.clear();
        Ok(state.status())
    }

    pub fn status(&self) -> AppResult<WatcherStatus> {
        let state = self
            .state
            .lock()
            .map_err(|_| AppError::Other("watcher state poisoned".to_string()))?;
        Ok(state.status())
    }
}

impl WatcherState {
    fn status(&self) -> WatcherStatus {
        WatcherStatus {
            running: self.watcher.is_some(),
            watched_roots: self.watched_roots.clone(),
            skipped_network_roots: self.skipped_network_roots.clone(),
            last_error: self.last_error.clone(),
        }
    }
}

fn handle_event(
    event: notify::Result<Event>,
    roots: &[Root],
    pool: &Pool,
    scheduler: &Scheduler,
    app: &AppHandle<tauri::Wry>,
    paths: &AppPaths,
    debounce: &Arc<Mutex<HashMap<i64, Instant>>>,
) -> AppResult<()> {
    let event = event.map_err(|error| AppError::Other(format!("watcher: {error}")))?;
    if !is_interesting_event(&event.kind) {
        return Ok(());
    }
    for event_path in event.paths {
        if let Some(root) = root_for_path(roots, &event_path) {
            if !debounced(root.id, debounce)? {
                continue;
            }
            let result = scan_service::create_scan_task(pool, root.id)?;
            scheduler.enqueue(ScanRootTask::new(
                pool.clone(),
                scheduler.clone(),
                app.clone(),
                paths.clone(),
                root.id,
                result.task_id,
            ))?;
        }
    }
    Ok(())
}

fn root_for_path<'a>(roots: &'a [Root], path: &Path) -> Option<&'a Root> {
    roots
        .iter()
        .find(|root| path.starts_with(Path::new(&root.path)))
}

fn debounced(root_id: i64, debounce: &Arc<Mutex<HashMap<i64, Instant>>>) -> AppResult<bool> {
    let now = Instant::now();
    let mut debounce = debounce
        .lock()
        .map_err(|_| AppError::Other("watcher debounce poisoned".to_string()))?;
    if debounce
        .get(&root_id)
        .is_some_and(|last| now.duration_since(*last) < WATCH_DEBOUNCE)
    {
        return Ok(false);
    }
    debounce.insert(root_id, now);
    Ok(true)
}

fn is_interesting_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}
