/*
 * JuiceFS, Copyright 2026 Juicedata, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::error::Result;
use io_uring::{opcode, types, types::Fd, IoUring};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use tokio::sync::oneshot;

pub static SIMULATE_CORRUPTION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test/fault-injection: next N `write_block` calls fail before I/O.
/// Used by layout atomicity tests (P0-2) and related regression suites.
pub static FAIL_NEXT_WRITES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn set_simulate_corruption(val: bool) {
    SIMULATE_CORRUPTION.store(val, std::sync::atomic::Ordering::Relaxed);
}

pub fn set_fail_next_writes(n: usize) {
    FAIL_NEXT_WRITES.store(n, std::sync::atomic::Ordering::SeqCst);
}

pub fn clear_fail_next_writes() {
    FAIL_NEXT_WRITES.store(0, std::sync::atomic::Ordering::SeqCst);
}

struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

struct SendConstPtr(*const u8);
unsafe impl Send for SendConstPtr {}
unsafe impl Sync for SendConstPtr {}

enum WriteData {
    Aligned {
        ptr: SendConstPtr,
        len: usize,
    },
    /// Heap buffer from `posix_memalign` — free with `libc::free`.
    Unaligned {
        ptr: SendPtr,
        len: usize,
    },
    /// Buffer from [`crate::cache::ALIGNED_BUF_POOL`] — recycle on completion (P2-4).
    PooledUnaligned {
        ptr: SendPtr,
        len: usize,
    },
}

fn release_write_buf(data: &WriteData) {
    match data {
        WriteData::Aligned { .. } => {}
        WriteData::Unaligned { ptr, .. } => {
            if !ptr.0.is_null() {
                unsafe {
                    libc::free(ptr.0 as *mut libc::c_void);
                }
            }
        }
        WriteData::PooledUnaligned { ptr, .. } => {
            crate::cache::ALIGNED_BUF_POOL.recycle(ptr.0);
        }
    }
}

fn release_free_ptr(kind: FreePtrKind, p: SendPtr) {
    match kind {
        FreePtrKind::Libc => {
            if !p.0.is_null() {
                unsafe {
                    libc::free(p.0 as *mut libc::c_void);
                }
            }
        }
        FreePtrKind::Pool => {
            crate::cache::ALIGNED_BUF_POOL.recycle(p.0);
        }
    }
}

#[derive(Clone, Copy)]
enum FreePtrKind {
    Libc,
    Pool,
}

enum UringRequest {
    Read {
        offset: u64,
        buf_ptr: SendPtr,
        size: usize,
        bytes: bytes::Bytes,
        tx: oneshot::Sender<Result<bytes::Bytes>>,
    },
    Write {
        offset: u64,
        data: WriteData,
        tx: oneshot::Sender<Result<()>>,
    },
}

enum UringResponse {
    Read {
        bytes: bytes::Bytes,
        size: usize,
        tx: oneshot::Sender<Result<bytes::Bytes>>,
    },
    Write {
        tx: oneshot::Sender<Result<()>>,
    },
}

struct UringWorker {
    /// Dropped first in `Drop` so the worker observes disconnect and drains.
    tx: Option<crossbeam::channel::Sender<UringRequest>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// P1-6: bound io_uring request queue to apply backpressure under overload.
const URING_REQ_QUEUE_CAP: usize = 4096;

impl UringWorker {
    fn new(device_path: String) -> Self {
        let (tx, rx) = crossbeam::channel::bounded(URING_REQ_QUEUE_CAP);
        let thread = std::thread::spawn(move || {
            worker_thread_loop(device_path, rx);
        });
        Self {
            tx: Some(tx),
            thread: Some(thread),
        }
    }

    fn sender(&self) -> &crossbeam::channel::Sender<UringRequest> {
        self.tx
            .as_ref()
            .expect("UringWorker sender used after drop")
    }
}

impl Drop for UringWorker {
    fn drop(&mut self) {
        // Close the channel first so the worker stops accepting work and exits
        // its loop, then join so exit cleanup (free unaligned bufs) runs before
        // we return (P0-1).
        drop(self.tx.take());
        if let Some(handle) = self.thread.take() {
            if let Err(e) = handle.join() {
                log::error!("NvmeBlockDev uring worker thread panicked: {:?}", e);
            }
        }
    }
}

fn worker_thread_loop(device_path: String, rx: crossbeam::channel::Receiver<UringRequest>) {
    let mut open_opts = OpenOptions::new();
    open_opts.read(true).write(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.custom_flags(libc::O_DIRECT);
    }

    let file = match open_opts.open(&device_path) {
        Ok(f) => f,
        Err(_) => {
            log::warn!(
                "WARNING: Failed to open NVMe device {:?} with O_DIRECT. Falling back to standard buffered I/O.",
                device_path
            );
            let mut open_opts = OpenOptions::new();
            open_opts.read(true).write(true);
            match open_opts.open(&device_path) {
                Ok(f) => f,
                Err(e) => {
                    log::error!("Failed to open NVMe device {:?}: {:?}", device_path, e);
                    return;
                }
            }
        }
    };

    let fd = file.as_raw_fd();

    let mut ring = match IoUring::new(1024) {
        Ok(r) => r,
        Err(e) => {
            log::error!("Failed to initialize worker io_uring: {:?}", e);
            return;
        }
    };

    // P2-8: register the block device as a fixed file so hot SQEs can use
    // Fixed(0) and avoid per-op fd lookup overhead. Fall back to plain Fd(fd)
    // if the kernel rejects registration.
    let use_fixed = match unsafe { ring.submitter().register_files(&[fd]) } {
        Ok(()) => {
            log::debug!(
                "NvmeBlockDev: registered fixed file for {:?} (index 0)",
                device_path
            );
            true
        }
        Err(e) => {
            log::debug!(
                "NvmeBlockDev: fixed-file register failed for {:?}: {:?} (using raw Fd)",
                device_path,
                e
            );
            false
        }
    };

    struct ActiveReq {
        response: UringResponse,
        free_ptr: Option<(FreePtrKind, SendPtr)>,
    }

    let mut active: Vec<Option<ActiveReq>> = Vec::with_capacity(1024);
    let mut free_slots: Vec<usize> = Vec::new();
    let mut active_count = 0;
    let mut disconnected = false;

    loop {
        let mut pushed = 0;
        loop {
            let req = if active_count == 0 {
                match rx.recv() {
                    Ok(r) => Some(r),
                    Err(_) => {
                        disconnected = true;
                        None
                    }
                }
            } else {
                match rx.try_recv() {
                    Ok(r) => Some(r),
                    Err(crossbeam::channel::TryRecvError::Empty) => None,
                    Err(crossbeam::channel::TryRecvError::Disconnected) => {
                        disconnected = true;
                        None
                    }
                }
            };

            let req = match req {
                Some(r) => r,
                None => break,
            };

            let slot_idx = match free_slots.pop() {
                Some(idx) => {
                    active[idx] = None;
                    idx
                }
                None => {
                    let idx = active.len();
                    active.push(None);
                    idx
                }
            };

            let sqe = match req {
                UringRequest::Read {
                    offset,
                    buf_ptr,
                    size,
                    bytes,
                    tx,
                } => {
                    active[slot_idx] = Some(ActiveReq {
                        response: UringResponse::Read { bytes, size, tx },
                        free_ptr: None,
                    });
                    if use_fixed {
                        opcode::Read::new(types::Fixed(0), buf_ptr.0, size as _)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64)
                    } else {
                        opcode::Read::new(Fd(fd), buf_ptr.0, size as _)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64)
                    }
                }
                UringRequest::Write { offset, data, tx } => {
                    let (ptr, len, free_ptr) = match data {
                        WriteData::Aligned { ptr, len } => (ptr.0, len, None),
                        WriteData::Unaligned { ptr, len } => {
                            (ptr.0 as *const u8, len, Some((FreePtrKind::Libc, ptr)))
                        }
                        WriteData::PooledUnaligned { ptr, len } => {
                            (ptr.0 as *const u8, len, Some((FreePtrKind::Pool, ptr)))
                        }
                    };
                    active[slot_idx] = Some(ActiveReq {
                        response: UringResponse::Write { tx },
                        free_ptr,
                    });
                    if use_fixed {
                        opcode::Write::new(types::Fixed(0), ptr, len as _)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64)
                    } else {
                        opcode::Write::new(Fd(fd), ptr, len as _)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64)
                    }
                }
            };

            unsafe {
                if let Err(e) = ring.submission().push(&sqe) {
                    log::error!("Failed to push SQE to io_uring: {:?}", e);
                    if let Some(act) = active[slot_idx].take() {
                        match act.response {
                            UringResponse::Read { tx, .. } => {
                                let _ = tx.send(Err(crate::error::SqueezefsError::Io(
                                    std::io::Error::new(
                                        std::io::ErrorKind::Other,
                                        "Submission queue full",
                                    ),
                                )));
                            }
                            UringResponse::Write { tx } => {
                                let _ = tx.send(Err(crate::error::SqueezefsError::Io(
                                    std::io::Error::new(
                                        std::io::ErrorKind::Other,
                                        "Submission queue full",
                                    ),
                                )));
                            }
                        }
                        if let Some((kind, p)) = act.free_ptr {
                            release_free_ptr(kind, p);
                        }
                    }
                    free_slots.push(slot_idx);
                    break;
                }
            }

            active_count += 1;
            pushed += 1;
        }

        if pushed > 0 {
            if let Err(e) = ring.submit() {
                log::error!("io_uring submit failed: {:?}", e);
            }
        }

        if active_count > 0 {
            if let Err(e) = ring.submit_and_wait(1) {
                log::error!("io_uring submit_and_wait failed: {:?}", e);
            }

            let mut cq = ring.completion();
            cq.sync();

            let mut completed_slots = Vec::new();
            for cqe in cq {
                let slot_idx = cqe.user_data() as usize;
                let res = cqe.result();

                if let Some(act) = active[slot_idx].take() {
                    let io_res = if res < 0 {
                        Err(std::io::Error::from_raw_os_error(-res))
                    } else {
                        Ok(res as usize)
                    };

                    match act.response {
                        UringResponse::Read { bytes, size, tx } => {
                            let mapped = io_res
                                .map_err(crate::error::SqueezefsError::Io)
                                .map(|_| bytes.slice(0..size));
                            let _ = tx.send(mapped);
                        }
                        UringResponse::Write { tx } => {
                            let mapped =
                                io_res.map(|_| ()).map_err(crate::error::SqueezefsError::Io);
                            let _ = tx.send(mapped);
                        }
                    }

                    if let Some((kind, p)) = act.free_ptr {
                        release_free_ptr(kind, p);
                    }
                }

                completed_slots.push(slot_idx);
            }

            for idx in completed_slots {
                free_slots.push(idx);
                active_count -= 1;
            }
        }

        if disconnected && active_count == 0 {
            break;
        }
    }

    // P0-1: Worker exit cleanup — free any unaligned write buffers still held
    // in-flight or left on the channel, and fail pending oneshots so callers
    // do not hang after NvmeBlockDev drop.
    while let Ok(req) = rx.try_recv() {
        match req {
            UringRequest::Write { data, tx, .. } => {
                release_write_buf(&data);
                let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                    "NvmeBlockDev worker shutting down".to_string(),
                )));
            }
            UringRequest::Read { tx, .. } => {
                let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                    "NvmeBlockDev worker shutting down".to_string(),
                )));
            }
        }
    }

    for slot in active.iter_mut() {
        if let Some(act) = slot.take() {
            let free_ptr = act.free_ptr;
            match act.response {
                UringResponse::Read { tx, .. } => {
                    let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                        "NvmeBlockDev worker shutting down".to_string(),
                    )));
                }
                UringResponse::Write { tx } => {
                    let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                        "NvmeBlockDev worker shutting down".to_string(),
                    )));
                }
            }
            if let Some((kind, p)) = free_ptr {
                release_free_ptr(kind, p);
            }
        }
    }
}

#[derive(Clone)]
pub struct NvmeBlockDev {
    pub device_path: String,
    worker: Arc<UringWorker>,
}

impl NvmeBlockDev {
    pub fn new(device_path: &str) -> Self {
        Self {
            device_path: device_path.to_string(),
            worker: Arc::new(UringWorker::new(device_path.to_string())),
        }
    }

    pub async fn write_block(&self, offset: u64, data: &[u8]) -> Result<()> {
        // Fault injection for atomicity / durability tests (no-op when counter is 0).
        loop {
            let cur = FAIL_NEXT_WRITES.load(std::sync::atomic::Ordering::SeqCst);
            if cur == 0 {
                break;
            }
            if FAIL_NEXT_WRITES
                .compare_exchange(
                    cur,
                    cur - 1,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::other(
                    "injected write_block failure (FAIL_NEXT_WRITES)",
                )));
            }
        }

        let data_len = data.len();
        let alignment = 4096;

        let rx_oneshot = if (data.as_ptr() as usize) % alignment == 0 && data_len % alignment == 0 {
            let data_ptr = data.as_ptr() as usize;
            let data_type = WriteData::Aligned {
                ptr: SendConstPtr(data_ptr as *const u8),
                len: data_len,
            };
            let (tx, rx) = oneshot::channel();
            self.worker
                .sender()
                .try_send(UringRequest::Write {
                    offset,
                    data: data_type,
                    tx,
                })
                .map_err(|e| {
                    crate::fuse_client::METRICS
                        .uring_queue_full
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "Uring request queue full or closed (backpressure): {:?}",
                        e
                    ))
                })?;
            rx
        } else {
            // P2-4: prefer the process-wide 4K-aligned buffer pool for typical
            // block sizes; fall back to posix_memalign only when the write is
            // larger than the pool buffer.
            let aligned_len = (data_len + 4095) & !4095;
            let pool = &crate::cache::ALIGNED_BUF_POOL;
            let use_pool = aligned_len <= pool.buf_size();

            let (rp, data_type) = if use_pool {
                let rp = pool.alloc_raw();
                unsafe {
                    libc::memcpy(
                        rp as *mut libc::c_void,
                        data.as_ptr() as *const libc::c_void,
                        data_len,
                    );
                    if aligned_len > data_len {
                        libc::memset(
                            (rp as usize + data_len) as *mut libc::c_void,
                            0,
                            aligned_len - data_len,
                        );
                    }
                }
                (
                    rp,
                    WriteData::PooledUnaligned {
                        ptr: SendPtr(rp),
                        len: aligned_len,
                    },
                )
            } else {
                let rp = unsafe {
                    let mut buf_ptr: *mut libc::c_void = std::ptr::null_mut();
                    if libc::posix_memalign(&mut buf_ptr, alignment, aligned_len) != 0 {
                        return Err(crate::error::SqueezefsError::InvalidOperation(
                            "posix_memalign failed for write block".to_string(),
                        ));
                    }
                    libc::memcpy(buf_ptr, data.as_ptr() as *const libc::c_void, data_len);
                    if aligned_len > data_len {
                        libc::memset(
                            (buf_ptr as usize + data_len) as *mut libc::c_void,
                            0,
                            aligned_len - data_len,
                        );
                    }
                    buf_ptr as *mut u8
                };
                (
                    rp,
                    WriteData::Unaligned {
                        ptr: SendPtr(rp),
                        len: aligned_len,
                    },
                )
            };
            let (tx, rx) = oneshot::channel();
            if self
                .worker
                .sender()
                .try_send(UringRequest::Write {
                    offset,
                    data: data_type,
                    tx,
                })
                .is_err()
            {
                // Send failed; worker never took ownership. Release immediately.
                if use_pool {
                    pool.recycle(rp);
                } else {
                    unsafe {
                        libc::free(rp as *mut libc::c_void);
                    }
                }
                crate::fuse_client::METRICS
                    .uring_queue_full
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Err(crate::error::SqueezefsError::InvalidOperation(
                    "Uring request queue full or closed (backpressure)".to_string(),
                ));
            }
            // rp raw pointer value was copied into SendPtr inside the sent message.
            // The local `rp` binding ends with this block; no !Send value crosses the await below.
            rx
        };

        rx_oneshot.await.map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Worker thread closed receiver: {:?}",
                e
            ))
        })??;

        crate::fuse_client::METRICS
            .put_obj
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // P2-9: full RAW only when verification is on *and* this write is sampled.
        if crate::write_verification_should_check() {
            let verified = self.verify_write_block(offset, data).await?;
            if !verified {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Write verification failed: checksum mismatch at offset {} on device {}",
                    offset, self.device_path
                )));
            }
        }

        Ok(())
    }

    pub async fn verify_write_block(&self, offset: u64, expected: &[u8]) -> Result<bool> {
        let read_bytes = self.read_block(offset, expected.len()).await?;
        let mut matched = read_bytes.as_ref() == expected;
        if matched && SIMULATE_CORRUPTION.load(std::sync::atomic::Ordering::Relaxed) {
            matched = false;
        }
        Ok(matched)
    }

    pub async fn read_block(&self, offset: u64, size: usize) -> Result<bytes::Bytes> {
        let (buf_ptr, bytes) = crate::cache::pool::ALIGNED_BUF_POOL.alloc();

        let (tx, rx_oneshot) = oneshot::channel();
        self.worker
            .sender()
            .try_send(UringRequest::Read {
                offset,
                buf_ptr: SendPtr(buf_ptr),
                size,
                bytes,
                tx,
            })
            .map_err(|e| {
                crate::fuse_client::METRICS
                    .uring_queue_full
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "Uring request queue full or closed (backpressure): {:?}",
                    e
                ))
            })?;

        let res = rx_oneshot.await.map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Worker thread closed receiver: {:?}",
                e
            ))
        })??;

        crate::fuse_client::METRICS
            .get_obj
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(res)
    }
}
