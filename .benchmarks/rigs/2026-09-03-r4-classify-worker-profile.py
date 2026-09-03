#!/usr/bin/env python3
"""R-4 reap-thread economy: classify a `perf report --sort dso,sym -g none` flat dump of the f3-ur
workers into the R-4 ledger classes; print µs/op per class given ops/s
and the workers' measured busy fraction.

usage: classify.py <flat.txt> <worker_cpu_us_per_op>
The second arg is the row's worker CPU per op (Σ f3-ur CPU / ops) so the
percentages become µs.
"""
import re
import sys

CLASSES = [
    # (class, dso-filter, regex over symbol)
    ("k:sched/wake", "k", r"_raw_spin_unlock_irqrestore|try_to_wake_up|__schedule|schedule|futex|wake_up|ttwu|set_task_cpu|select_task_rq|enqueue_task|dequeue_task|pick_next|update_curr|__wake_up|wake_q|sched_|activate_task|check_preempt|resched_curr|native_sched_clock|psi_|__x64_sys_futex|do_futex|__rseq|rseq|__pi_clear_user|_raw_spin_unlock_irq$|switch_mm|finish_task_switch|__perf_event_task_sched|prepare_task_switch|task_work|__io_run_local_work|io_run_local_work|__wake_up_common|autoremove_wake|io_wake_function|_raw_spin_lock_irq"),
    ("k:fuse commit/fetch", "k", r"fuse_uring|fuse_request_end|fuse_put_request|fuse_aio|fuse_copy|fuse_dev|fuse_zc|fuse_readpages|fuse_read|fuse_send|fuse_get_req|fuse_simple|fuse_release_user|fuse_io_|fuse_uring_send|fuse_uring_commit|fuse_uring_prepare|fuse_uring_cmd|fuse_ent|fuse_"),
    ("k:io_uring core", "k", r"io_uring|io_submit|io_cqring|io_issue|io_req|io_prep|io_read|io_rw|io_import|io_fixed|io_alloc|io_free|io_queue|io_commit|__io_|io_uring_cmd|io_poll|io_arm_poll|io_eventfd|io_file|io_iopoll|io_wq|io_get_cqe|io_fill_cqe|io_post|io_do_iopoll|io_disarm|io_kill|io_ring|io_task_work|io_req_task|io_apoll|io_async"),
    ("k:nvme/blk submit", "k", r"nvme|blk_|bio_|blkdev|__blkdev|submit_bio|generic_make|iomap|dio_|__x64_sys_pread|filemap|kiocb|iocb|aio_|sock_sendmsg|tcp_|inet_|ip_|__sk|sk_|skb|net_|dev_queue|__dev_|loopback|eth|ipv4|__tcp|tcp|ksoftirqd|__do_softirq|net_rx|napi|ip6|queue_work_on|__queue_work|insert_work|kick_pool|wq_|workqueue|process_one_work|kthread|bpf_prog|sd_fw|__local_bh|do_softirq|irq_exit|__irq|handle_edge|handle_irq|sysvec|asm_sysvec|common_interrupt|__sysvec|native_apic|lapic|hrtimer|tick_|ktime|clockevents|timerqueue|__run_timer|call_timer|expire_timers|run_timer|__hrtimer|ktime_get"),
    ("k:mm/slab/memcg", "k", r"memcg|slab|kmem|kfree|kmalloc|__alloc|alloc_pages|__free_pages|free_unref|page_|__get_free|mm_|__folio|folio|get_page|put_page|rmqueue|__rmqueue|zone|obj_cgroup|refill|__slab|___slab|slub|cgroup|__mod_|mod_memcg|lruvec|__mem_cgroup|charge|uncharge|copy_user|__copy|_copy_from|_copy_to|clear_page|memset|memcpy|__pi_memcpy|__memcpy|__pi_memset|rep_movs|__memmove|iov_iter|copy_page|__iov|import_ubuf|import_iovec|fixed_buf|unpin|pin_user|gup|__gup|follow_page|get_user_pages|__get_user|__put_user|put_user|__might|kasan|__check|check_object|__virt|virt_to|kmap|kunmap"),
    ("k:syscall entry/exit", "k", r"do_syscall_64|entry_SYSCALL|syscall_exit|syscall_enter|__x64_sys|exit_to_user|syscall_return|__fget|fget|fput|__fdget|fdget|__se_sys|__do_sys|x64_sys_call|do_sys|__x64|ret_from|error_entry|sync_regs|irqentry|__fdget_pos|__fput|_raw_spin_lock$|_raw_spin_lock_bh|_raw_spin_unlock$|_raw_spin_unlock_bh|__rcu|rcu_|srcu|__srcu|percpu|this_cpu|__percpu|__cond_resched|cond_resched|__might_sleep|preempt|__preempt|lock_|__lock|unlock|mutex|__mutex|rwsem|down_read|up_read|down_write|up_write|_raw_read|_raw_write|__list|list_|hlist|__hash|hash|rb_|__rb|idr|xa_|xas_|radix|__xa|find_next|find_first|bitmap|__bitmap|memchr|strlen|strnlen|strcmp|__strn|__strc|strncpy"),
    ("d:queue_worker self", "d", r"queue_worker$|queue_worker::\{closure|fuse_over_uring::queue_worker::commit_ready_reply|commit_ready_reply|zc_fetch_complete|apply_reply|submit_commit|push_cmd|push_fetch|build_cmd_entry|decode_user_data|encode_user_data|SlotTable|slot_watch|publish_slot_owed|MemberState|member_of_qid"),
    ("d:worker instruments", "d", r"ReapCadence|note_flush|note_bridge|settle_bridge|note_handler_bridge|note_handler_fetch|BridgeDeadlines|record_commit_flush|read_phase::|reap_phase|zc_bridge_phase|transport_phase|PhaseHist|record_ns|latency_bucket|op_trace|CommitBatchHistogram|note_zc|note_fast|record_served|note_read_inplace|cadence|CqDropWatch|fast_dispatch::|fused::note|shard_index|transport_now_ns|numa_classify"),
    ("d:clock reads", "d", r"clock_gettime|__vdso|Instant::now|Instant>::now|elapsed|std::time|now\b"),
    ("d:probe ladder (FS)", "d", r"read_fast_probe|NvmeCache|try_read_range_sync|DataRouter|ReadMostlyCache|CachedMetadata|DashMap|ActiveBlockBuf|StackKey|from_utf8|hash|Hasher|hashbrown|scc::|hot_block|HotBlock|read_lane|ReadLane|staging|Staging|routing::|fuse_client::|SqueezefsFilesystem|tiering::|cache::|FileAttr|attr_cache|keys::|fmt::|format::|Display|write_str|core::fmt|LockCore|try_acquire|sqz_sync|RwLock|try_read|Mutex|pthread_mutex|peek_with|_get|get_static|metadata|Metadata|inode|Inode|layout|Layout|block_key|BlockKey|resolve|lookup|Lookup|probe|Probe"),
    ("d:dispatch/spawn (lane hand-off)", "d", r"LaneExec|spawn_boxed|LaneTask|Wake|wake|tpc_dispatch|tpc_spawn|mpmc|try_recv|Channel|channel|oneshot|Sender|Receiver|send\b|crossbeam|sqz_channel|sqz_exec|FusedLane|fused::|FusedFuture|Box<|drop_in_place|core::ptr::drop|Arc<|Arc>|inner_mount|read_handler_body|handle_read|mint|Session<|session::|dispatch|Dispatch|Pin<|poll|Poll|Future|future|Context|Waker|RawWaker|task::"),
    ("d:alloc (jemalloc)", "d", r"_rjem|je_|malloc|free$|realloc|calloc|alloc::|__rust_alloc|__rust_dealloc|__rdl_|RawVec|Vec<|vec::|Bytes|bytes::|from_owner|copy_from_slice|shared_|Shared|String"),
    ("d:memmove/copy", "d", r"memmove|memcpy|__memmove|__memcpy|copy_nonoverlapping|ptr::copy|__memset|memset|nt_copy|nt_store"),
    ("d:syscall stub (libc)", "d", r"^syscall$|syscall@|__GI___|GLIBC|libc|pthread|__libc|__errno|__tls|__pthread"),
]

samples = []
tot = 0.0
for line in open(sys.argv[1]):
    m = re.match(r"\s*([\d.]+)%\s+(\S.*?)\s{2,}(\[[.k]\])\s+(.*)$", line.rstrip())
    if not m:
        m2 = re.match(r"\s*([\d.]+)%\s+(\S+)\s+(\[[.k]\])\s+(.*)$", line.rstrip())
        if not m2:
            continue
        m = m2
    pct = float(m.group(1)); dso = m.group(2).strip(); kind = m.group(3); sym = m.group(4).strip()
    samples.append((pct, dso, kind, sym))
    tot += pct

us_per_op = float(sys.argv[2])
agg = {}
unk = []
for pct, dso, kind, sym in samples:
    isk = kind == "[k]" or "kernel" in dso
    cls = None
    for name, side, rx in CLASSES:
        if (side == "k") != isk:
            continue
        if re.search(rx, sym):
            cls = name
            break
    if cls is None:
        cls = "k:other" if isk else ("d:other(" + dso.split("/")[-1] + ")")
        unk.append((pct, dso, sym))
    agg[cls] = agg.get(cls, 0.0) + pct

print(f"classified {tot:.1f}% of samples; worker CPU {us_per_op:.2f} us/op")
ksum = sum(v for k, v in agg.items() if k.startswith("k:"))
dsum = sum(v for k, v in agg.items() if k.startswith("d:"))
print(f"kernel {ksum:.1f}% = {ksum/100*us_per_op:.2f} us/op ; daemon {dsum:.1f}% = {dsum/100*us_per_op:.2f} us/op")
for k, v in sorted(agg.items(), key=lambda x: -x[1]):
    print(f"  {k:36s} {v:6.2f}%  {v/100*us_per_op:6.2f} us/op")
print("\nunclassified top:")
for pct, dso, sym in sorted(unk, reverse=True)[:25]:
    print(f"  {pct:5.2f}%  {dso:20s} {sym[:110]}")
