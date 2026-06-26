use std::time::Duration;

#[test]
fn test_cpu_affinity_pinning_reservation() {
    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
    if core_ids.len() < 2 {
        println!("Skipping test: not enough CPU cores to test pinning reservation");
        return;
    }

    // Exclude Core 0 from the list of cores to pin to
    let mut target_core_ids = core_affinity::get_core_ids().unwrap_or_default();
    if !target_core_ids.is_empty() {
        target_core_ids.remove(0); // Reserve Core 0
    }

    let core_counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pinned_cores = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let target_core_ids_clone = target_core_ids.clone();
    let pinned_cores_clone = pinned_cores.clone();

    // Spawn a runtime to verify thread pinning behavior
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(core_ids.len() - 1)
        .on_thread_start(move || {
            let idx = core_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if idx < target_core_ids_clone.len() {
                let core_id = target_core_ids_clone[idx];
                core_affinity::set_for_current(core_id);
                pinned_cores_clone.lock().unwrap().push(core_id);
            }
        })
        .build()
        .unwrap();

    rt.block_on(async {
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    let pinned = pinned_cores.lock().unwrap();
    let first_core = core_ids[0];

    // Core 0 must not be in the pinned cores list
    for &id in pinned.iter() {
        assert_ne!(
            id.id, first_core.id,
            "Core 0 (id {}) must not be pinned by worker threads",
            first_core.id
        );
    }
}
