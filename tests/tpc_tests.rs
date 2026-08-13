use fuse3::{tpc_spawn, tpc_thread_count};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_tpc_scheduler_threads_and_no_migration() {
    let threads = tpc_thread_count();
    println!("TPC thread count: {}", threads);
    assert!(threads > 0, "TPC scheduler should have at least 1 thread");

    let thread_ids = Arc::new(Mutex::new(HashSet::new()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);

    for _ in 0..threads * 4 {
        let thread_ids = thread_ids.clone();
        let tx = tx.clone();
        tpc_spawn(async move {
            let start_thread = std::thread::current().id();
            thread_ids.lock().unwrap().insert(start_thread);

            // Yield multiple times to check for task migration.
            // sqz_time, not tokio::time (rip-tokio-total): the TPC
            // lanes carry no tokio driver any more — a tokio sleep
            // inside a lane task panics "no reactor running", which is
            // the removal WORKING; the timer that serves lane tasks is
            // the sqz-timer thread.
            tokio::task::yield_now().await;
            fuse3::sqz_time::sleep(Duration::from_millis(5)).await;
            tokio::task::yield_now().await;

            let end_thread = std::thread::current().id();
            assert_eq!(start_thread, end_thread, "Task migrated between threads!");

            let _ = tx.send(()).await;
        });
    }

    // Drop the sender we hold in this task so rx closes when all spawns finish
    drop(tx);

    let mut count = 0;
    while rx.recv().await.is_some() {
        count += 1;
    }

    assert_eq!(count, threads * 4, "Not all tasks completed");

    let unique_threads = thread_ids.lock().unwrap().len();
    println!("Unique threads handling tasks: {}", unique_threads);
    // Since we round-robin tasks, we expect them to be distributed across worker threads
    assert!(unique_threads <= threads);
    if threads > 1 {
        assert!(
            unique_threads > 1,
            "Tasks should be distributed across multiple threads"
        );
    }
}
