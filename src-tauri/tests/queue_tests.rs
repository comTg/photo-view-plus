use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use photo_view_plus_lib::queue::{FnTask, Priority, Scheduler};

#[tokio::test]
async fn higher_priority_runs_first() {
    let scheduler = Scheduler::start(1);
    scheduler.pause();
    let order = Arc::new(Mutex::new(Vec::new()));

    for (priority, value) in [(Priority::P3, 3), (Priority::P0, 0), (Priority::P1, 1)] {
        let order = order.clone();
        scheduler
            .enqueue(FnTask::new(priority, format!("task-{value}"), move |_| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().expect("record order").push(value);
                    Ok(())
                })
            }))
            .expect("enqueue");
    }

    scheduler.resume();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*order.lock().expect("order"), vec![0, 1, 3]);
}

#[tokio::test]
async fn cancel_token_reaches_running_task() {
    let scheduler = Scheduler::start(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();

    scheduler
        .enqueue(FnTask::new(Priority::P0, "cancel-check", move |ctx| {
            let flag = flag.clone();
            Box::pin(async move {
                for _ in 0..20 {
                    if ctx.is_cancelled() {
                        flag.store(true, Ordering::Relaxed);
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Ok(())
            })
        }))
        .expect("enqueue");

    tokio::time::sleep(Duration::from_millis(50)).await;
    scheduler.cancel_running();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(cancelled.load(Ordering::Relaxed));
}
