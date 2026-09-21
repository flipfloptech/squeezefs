//! PR 13b — the HOLDER's kernel after a served record-level verb or
//! layout publish (the `sym-foreign-file` leg's second finding).
//!
//! The finding: a colleague's served inline append landed at the holder's
//! KV (`foreign_publish_served` 1, `PutDone`), the holder daemon's own
//! caches were invalidated, the holder's kernel was told
//! `FUSE_NOTIFY_INVAL_INODE` — and the holder still read the OLD 3 bytes
//! for the inode's life (`stat` size 3, every other mount 7; `echo 2 >
//! drop_caches` cured it). The law it hit is the kernel's: under
//! `FUSE_WRITEBACK_CACHE` (the default mount's negotiation)
//! `fuse_get_cache_mask` makes a cached regular inode's size / mtime /
//! ctime the KERNEL's for its life — every attr reply's size is replaced
//! by `i_size_read(inode)` — and `fuse_reverse_inval_inode` touches attrs
//! and pages, never `i_size`. The ONE kernel act that makes such a kernel
//! adopt a peer mount's change is `FUSE_NOTIFY_PRUNE` (uapi 7.45,
//! `d_prune_aliases`): an inode nobody holds open loses its dentries,
//! `generic_delete_inode` evicts it, and its next lookup re-instantiates
//! it from the daemon's attrs.
//!
//! The pins assert the daemon's served-mutation kernel hook delivers
//! exactly that — the invalidation frame first (the pages and the attrs
//! of an inode a process HOLDS OPEN, which the prune cannot evict), then
//! the prune — through the fork's ONE shared encoders, synchronously, and
//! that a kernel without the notification (or a mount without the
//! writeback cache, where the invalidation alone re-syncs everything)
//! gets the invalidation alone.

use arc_swap::ArcSwap;
use squeezefs::fuse_client::make_served_mutation_kernel_hook;
use squeezefs::meta_backend::record_ship::ServedMutation;
use squeezefs::meta_backend::Metadata;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

mod common;
use common::sym::{format_flat_member, open_under, shutdown, Knobs};

fn hook_with(
    prune: bool,
) -> (
    Arc<dyn Fn(u64, ServedMutation) + Send + Sync>,
    fuse3::notify::NotifyTestRx,
) {
    let (notify, rx) = fuse3::notify::notify_test_channel();
    let cell = Arc::new(ArcSwap::from_pointee(Some(notify)));
    let hook = make_served_mutation_kernel_hook(cell, Arc::new(move || prune));
    (hook, rx)
}

/// A served layout publish (the bytes and the size moved): the WHOLE
/// inode invalidation `(0, 0)` — attrs + every page — then the prune, so
/// an unreferenced inode is re-instantiated at the served size.
#[test]
fn a_served_data_mutation_invalidates_the_whole_inode_then_prunes_it() {
    let (hook, mut rx) = hook_with(true);
    hook(772, ServedMutation::Data);
    assert_eq!(
        rx.try_next_frame()
            .expect("the invalidation is enqueued synchronously"),
        fuse3::notify::inval_inode_frame(772, 0, 0),
        "a served publish drops the kernel's attrs AND pages (off 0, len 0 = the whole file)"
    );
    assert_eq!(
        rx.try_next_frame()
            .expect("the prune follows in the same call"),
        fuse3::notify::prune_frame(&[772]),
        "FUSE_NOTIFY_PRUNE of exactly the served ino"
    );
    assert!(rx.try_next_frame().is_none(), "two frames, nothing else");
}

/// A served record verb (mode / owner / times / xattrs — no bytes moved):
/// the attrs-only invalidation `(-1, 0)` (no page work), then the prune —
/// mtime and ctime are the writeback kernel's too.
#[test]
fn a_served_attrs_mutation_invalidates_attrs_only_then_prunes() {
    let (hook, mut rx) = hook_with(true);
    hook(9, ServedMutation::Attrs);
    assert_eq!(
        rx.try_next_frame().expect("attrs-only invalidation"),
        fuse3::notify::inval_inode_frame(9, -1, 0),
    );
    assert_eq!(
        rx.try_next_frame().expect("the prune"),
        fuse3::notify::prune_frame(&[9]),
    );
    assert!(rx.try_next_frame().is_none());
}

/// A kernel below uapi 7.45, or a mount without the writeback cache: the
/// invalidation alone (a pre-7.45 kernel answers the prune frame EINVAL;
/// without the writeback cache the invalidated attrs re-sync size and
/// times at the next GETATTR by themselves).
#[test]
fn without_prune_capability_only_the_invalidation_travels() {
    let (hook, mut rx) = hook_with(false);
    hook(5, ServedMutation::Data);
    assert_eq!(
        rx.try_next_frame().expect("the invalidation"),
        fuse3::notify::inval_inode_frame(5, 0, 0),
    );
    assert!(
        rx.try_next_frame().is_none(),
        "no prune frame on an incapable kernel"
    );
}

/// The venue law (`tests/ipc_inval_venue_tests.rs`'s): the hook runs on
/// whatever thread served the verb — a plain OS thread with no runtime
/// must deliver both frames before the call returns.
#[test]
fn the_hook_delivers_from_a_plain_os_thread_without_a_runtime() {
    let (hook, mut rx) = hook_with(true);
    std::thread::spawn(move || hook(77, ServedMutation::Data))
        .join()
        .expect("no runtime is required on the firing thread");
    assert!(rx.try_next_frame().is_some());
    assert!(rx.try_next_frame().is_some());
}

/// Before the session exists (the cell holds no `Notify`) the hook is a
/// no-op — nothing to tell, nothing panics.
#[test]
fn the_hook_skips_pre_mount_fires() {
    let cell = Arc::new(ArcSwap::from_pointee(None));
    let hook = make_served_mutation_kernel_hook(cell, Arc::new(|| true));
    hook(1, ServedMutation::Attrs);
}

/// **A FLAT / unarmed mount pushes nothing new**: with the served-mutation
/// sink installed, a whole writer session of the mutations the ship
/// serves — `setattr`, `setxattr`, `removexattr`, a layout publish — on
/// an unarmed flat volume never reaches the sink (no served verb, no
/// armed slot lease: the two gates `record_ship::note_served` and
/// `PublishService::note_foreign_publish_served` both read closed). The
/// kernel invalidations this rung adds are the armed holder's alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unarmed_flat_writer_never_reaches_the_served_mutation_sink() {
    use squeezefs::layout_wire::{encode_layout, LayoutMetadata};
    let fired = Arc::new(AtomicU64::new(0));
    let f = Arc::clone(&fired);
    squeezefs::meta_backend::record_ship::install_served_mutation_sink(Arc::new(
        move |_ino, _kind| {
            let f = Arc::clone(&f);
            Box::pin(async move {
                f.fetch_add(1, Ordering::Relaxed);
            })
        },
    ));
    let dir = tempfile::tempdir().unwrap();
    let uri = format_flat_member(dir.path(), "flat0").await;
    let knobs = Knobs::unarmed();
    let routed = open_under(std::slice::from_ref(&uri), &knobs).await;
    let ino = Metadata::create(routed.as_ref(), 1, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    Metadata::setattr(
        routed.as_ref(),
        ino,
        Some(0o640),
        None,
        None,
        None,
        None,
        Some(1_700_000_000_000_000_000),
        Some(1_700_000_000_000_000_000),
    )
    .await
    .unwrap();
    Metadata::setxattr(routed.as_ref(), ino, "user.ff", b"x")
        .await
        .unwrap();
    Metadata::removexattr(routed.as_ref(), ino, "user.ff")
        .await
        .unwrap();
    let inline = LayoutMetadata {
        file_type: "inline".to_string(),
        size: 7,
        block_map_id: None,
        block_prefix: None,
        file_id: None,
        data_key: Some(b"abcDEFG".to_vec()),
        block_map: None,
    };
    routed
        .set_layout_and_size(ino, &encode_layout(&inline).unwrap(), 7, &[])
        .await
        .unwrap();
    assert_eq!(
        fired.load(Ordering::Relaxed),
        0,
        "an unarmed flat writer's own mutations never reach the served-mutation sink"
    );
    shutdown(&routed).await;
}
