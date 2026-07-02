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
use io_uring::{opcode, types::Fd, IoUring};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use tokio::sync::oneshot;

pub static SIMULATE_CORRUPTION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_simulate_corruption(val: bool) {
    SIMULATE_CORRUPTION.store(val, std::sync::atomic::Ordering::Relaxed);
}

struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

struct SendConstPtr(*const u8);
unsafe impl Send for SendConstPtr {}
unsafe impl Sync for SendConstPtr {}

enum WriteData {
    Aligned { ptr: SendConstPtr, len: usize },
    Unaligned { ptr: SendPtr, len: usize },
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
    tx: crossbeam::channel::Sender<UringRequest>,
    _thread: std::thread::JoinHandle<()>,
}

impl UringWorker {
    fn new(device_path: String) -> Self {
        let (tx, rx) = crossbeam::channel::unbounded();
        let thread = std::thread::spawn(move || {
            worker_thread_loop(device_path, rx);
        });
        Self {
            tx,
            _thread: thread,
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

    struct ActiveReq {
        response: UringResponse,
        free_ptr: Option<SendPtr>,
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
                    opcode::Read::new(Fd(fd), buf_ptr.0, size as _)
                        .offset(offset)
                        .build()
                        .user_data(slot_idx as u64)
                }
                UringRequest::Write { offset, data, tx } => {
                    let (ptr, len, free_ptr) = match data {
                        WriteData::Aligned { ptr, len } => (ptr.0, len, None),
                        WriteData::Unaligned { ptr, len } => (ptr.0 as *const u8, len, Some(ptr)),
                    };
                    active[slot_idx] = Some(ActiveReq {
                        response: UringResponse::Write { tx },
                        free_ptr,
                    });
                    opcode::Write::new(Fd(fd), ptr, len as _)
                        .offset(offset)
                        .build()
                        .user_data(slot_idx as u64)
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
                        if let Some(p) = act.free_ptr {
                            libc::free(p.0 as *mut libc::c_void);
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

                    if let Some(p) = act.free_ptr {
                        unsafe { libc::free(p.0 as *mut libc::c_void) };
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
        let data_len = data.len();
        let alignment = 4096;

        let data_type = if (data.as_ptr() as usize) % alignment == 0 && data_len % alignment == 0 {
            let data_ptr = data.as_ptr() as usize;
            WriteData::Aligned {
                ptr: SendConstPtr(data_ptr as *const u8),
                len: data_len,
            }
        } else {
            let aligned_len = (data_len + 4095) & !4095;
            let mut buf_ptr: *mut libc::c_void = std::ptr::null_mut();
            unsafe {
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
            }
            WriteData::Unaligned {
                ptr: SendPtr(buf_ptr as *mut u8),
                len: aligned_len,
            }
        };

        let (tx, rx_oneshot) = oneshot::channel();
        self.worker
            .tx
            .send(UringRequest::Write {
                offset,
                data: data_type,
                tx,
            })
            .map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "Failed to send write request to worker: {:?}",
                    e
                ))
            })?;

        rx_oneshot.await.map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Worker thread closed receiver: {:?}",
                e
            ))
        })??;

        crate::fuse_client::METRICS
            .put_obj
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        if crate::write_verification_enabled() {
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
            .tx
            .send(UringRequest::Read {
                offset,
                buf_ptr: SendPtr(buf_ptr),
                size,
                bytes,
                tx,
            })
            .map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "Failed to send read request to worker: {:?}",
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
