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
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;

static RING_POOL: once_cell::sync::Lazy<
    crossbeam::queue::ArrayQueue<std::sync::Arc<std::sync::Mutex<IoUring>>>,
> = once_cell::sync::Lazy::new(|| {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(16);
    let pool = crossbeam::queue::ArrayQueue::new(cores);
    for _ in 0..cores {
        if let Ok(ring) = IoUring::builder().build(256) {
            let _ = pool.push(std::sync::Arc::new(std::sync::Mutex::new(ring)));
        }
    }
    pool
});

fn with_ring<F, T>(f: F) -> Result<T>
where
    F: FnOnce(&mut IoUring) -> std::io::Result<T>,
{
    let ring_arc = RING_POOL.pop().ok_or_else(|| {
        crate::error::SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "io_uring pool empty",
        ))
    })?;
    let mut ring = ring_arc.lock().unwrap();
    let result = f(&mut ring);
    drop(ring);
    let _ = RING_POOL.push(ring_arc);
    result.map_err(crate::error::SqueezefsError::Io)
}

pub static SIMULATE_CORRUPTION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_simulate_corruption(val: bool) {
    SIMULATE_CORRUPTION.store(val, std::sync::atomic::Ordering::Relaxed);
}

#[derive(Clone)]
pub struct NvmeBlockDev {
    pub device_path: String,
    file_pool: std::sync::Arc<once_cell::sync::OnceCell<crossbeam::queue::ArrayQueue<File>>>,
}

impl NvmeBlockDev {
    pub fn new(device_path: &str) -> Self {
        Self {
            device_path: device_path.to_string(),
            file_pool: std::sync::Arc::new(once_cell::sync::OnceCell::new()),
        }
    }

    fn get_pool(&self) -> &crossbeam::queue::ArrayQueue<File> {
        self.file_pool.get_or_init(|| {
            let cores = std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(4);
            let pool_size = std::cmp::max(cores * 2, 8);
            let queue = crossbeam::queue::ArrayQueue::new(pool_size);

            // Try to pre-populate with file handles
            for _ in 0..pool_size {
                if let Ok(file) = self.open_file_handle() {
                    let _ = queue.push(file);
                }
            }
            queue
        })
    }

    fn open_file_handle(&self) -> Result<File> {
        let mut std_file = {
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::fs::OpenOptionsExt;
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_DIRECT)
                    .open(&self.device_path)
            }
            #[cfg(not(target_os = "linux"))]
            {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.device_path)
            }
        };

        // Fallback to standard open without O_DIRECT if it fails
        if std_file.is_err() {
            log::warn!(
                "WARNING: Failed to open NVMe block device {:?} with O_DIRECT. Falling back to standard buffered I/O, which may pollute page cache.",
                self.device_path
            );
            std_file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.device_path);
        }

        std_file.map_err(|e| crate::error::SqueezefsError::Io(e))
    }

    fn borrow_file(&self) -> Result<File> {
        let pool = self.get_pool();
        if let Some(file) = pool.pop() {
            Ok(file)
        } else {
            self.open_file_handle()
        }
    }

    fn return_file(&self, file: File) {
        let pool = self.get_pool();
        let _ = pool.push(file);
    }

    pub async fn write_block(&self, offset: u64, data: &[u8]) -> Result<()> {
        let file = self.borrow_file()?;
        let data_len = data.len();
        let alignment = 4096;

        // If the data is already page-aligned and is a multiple of 4096 bytes, we bypass allocation and copying completely!
        if (data.as_ptr() as usize) % alignment == 0 && data_len % alignment == 0 {
            let data_ptr = data.as_ptr() as usize;
            let (res, file) = tokio::task::spawn_blocking(move || {
                let aligned_slice =
                    unsafe { std::slice::from_raw_parts(data_ptr as *const u8, data_len) };

                let res = with_ring(|ring| {
                    let fd = Fd(file.as_raw_fd());
                    let write_e = opcode::Write::new(fd, aligned_slice.as_ptr(), data_len as _)
                        .offset(offset)
                        .build()
                        .user_data(1);
                    unsafe {
                        ring.submission().push(&write_e).map_err(|_| {
                            std::io::Error::new(std::io::ErrorKind::Other, "Submission queue full")
                        })?;
                    }
                    ring.submit_and_wait(1)?;

                    let mut cq = ring.completion();
                    cq.sync();
                    let mut write_res = Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "io_uring write CQE not found",
                    ));
                    for cqe in cq {
                        if cqe.user_data() == 1 {
                            let res = cqe.result();
                            write_res = if res < 0 {
                                Err(std::io::Error::from_raw_os_error(-res))
                            } else {
                                Ok(())
                            };
                            break;
                        }
                    }
                    write_res
                });
                (res, file)
            })
            .await
            .map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "Block write task panicked: {:?}",
                    e
                ))
            })?;
            self.return_file(file);
            res?;

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
            return Ok(());
        }

        let aligned_len = (data_len + 4095) & !4095;

        // Allocate page-aligned buffer once on calling thread
        let buf_addr = {
            let mut buf_ptr: *mut libc::c_void = std::ptr::null_mut();
            unsafe {
                if libc::posix_memalign(&mut buf_ptr, alignment, aligned_len) == 0 {
                    // Copy data directly to the aligned buffer once
                    libc::memcpy(buf_ptr, data.as_ptr() as *const libc::c_void, data_len);
                    if aligned_len > data_len {
                        libc::memset(
                            (buf_ptr as usize + data_len) as *mut libc::c_void,
                            0,
                            aligned_len - data_len,
                        );
                    }
                    buf_ptr as usize
                } else {
                    self.return_file(file);
                    return Err(crate::error::SqueezefsError::InvalidOperation(
                        "posix_memalign failed for write block".to_string(),
                    ));
                }
            }
        };

        let (res, file) = tokio::task::spawn_blocking(move || {
            let res = with_ring(|ring| {
                let fd = Fd(file.as_raw_fd());
                let write_e = opcode::Write::new(fd, buf_addr as *const u8, aligned_len as _)
                    .offset(offset)
                    .build()
                    .user_data(4);
                unsafe {
                    ring.submission().push(&write_e).map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::Other, "Submission queue full")
                    })?;
                }
                ring.submit_and_wait(1)?;

                let mut cq = ring.completion();
                cq.sync();
                let mut write_res = Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "io_uring write unaligned CQE not found",
                ));
                for cqe in cq {
                    if cqe.user_data() == 4 {
                        let res = cqe.result();
                        write_res = if res < 0 {
                            Err(std::io::Error::from_raw_os_error(-res))
                        } else {
                            Ok(())
                        };
                        break;
                    }
                }
                write_res
            });
            unsafe {
                libc::free(buf_addr as *mut libc::c_void);
            }
            (res, file)
        })
        .await
        .map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Thread join error on write_block: {}",
                e
            ))
        })?;
        self.return_file(file);
        res?;

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
        let file = self.borrow_file()?;
        let expected_len = expected.len();
        let aligned_len = (expected_len + 4095) & !4095;
        let alignment = 4096;
        let expected_ptr = expected.as_ptr() as usize;

        let (res, file) = tokio::task::spawn_blocking(move || {
            let mut buf_ptr: *mut libc::c_void = std::ptr::null_mut();
            unsafe {
                if libc::posix_memalign(&mut buf_ptr, alignment, aligned_len) != 0 {
                    return (
                        Err(crate::error::SqueezefsError::InvalidOperation(
                            "posix_memalign failed for verify_write_block".to_string(),
                        )),
                        file,
                    );
                }
            }

            let aligned_slice =
                unsafe { std::slice::from_raw_parts_mut(buf_ptr as *mut u8, aligned_len) };

            let res = with_ring(|ring| {
                let fd = Fd(file.as_raw_fd());
                let read_e = opcode::Read::new(fd, aligned_slice.as_mut_ptr(), expected_len as _)
                    .offset(offset)
                    .build()
                    .user_data(3);
                unsafe {
                    ring.submission().push(&read_e).map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::Other, "Submission queue full")
                    })?;
                }
                ring.submit_and_wait(1)?;

                let mut cq = ring.completion();
                cq.sync();
                let mut read_res = Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "io_uring verify CQE not found",
                ));
                for cqe in cq {
                    if cqe.user_data() == 3 {
                        let res = cqe.result();
                        read_res = if res < 0 {
                            Err(std::io::Error::from_raw_os_error(-res))
                        } else {
                            Ok(())
                        };
                        break;
                    }
                }
                read_res
            });

            let matched = if res.is_ok() {
                if SIMULATE_CORRUPTION.load(std::sync::atomic::Ordering::Relaxed) {
                    aligned_slice[0] ^= 0xFF;
                }
                let expected_slice =
                    unsafe { std::slice::from_raw_parts(expected_ptr as *const u8, expected_len) };
                aligned_slice[0..expected_len] == *expected_slice
            } else {
                false
            };

            unsafe {
                libc::free(buf_ptr);
            }
            (Ok(matched), file)
        })
        .await
        .map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Thread join error on verify_write_block: {}",
                e
            ))
        })?;
        self.return_file(file);
        res
    }

    pub async fn read_block(&self, offset: u64, size: usize) -> Result<bytes::Bytes> {
        let file = self.borrow_file()?;
        let (res, file) = tokio::task::spawn_blocking(move || {
            let buffer = vec![0u8; size + 4096];
            let ptr = buffer.as_ptr() as usize;
            let aligned_ptr = (ptr + 4095) & !4095;
            let align_offset = aligned_ptr - ptr;
            let buf_ptr = aligned_ptr as *mut u8;

            let res = with_ring(|ring| {
                let fd = Fd(file.as_raw_fd());
                let read_e = opcode::Read::new(fd, buf_ptr, size as _)
                    .offset(offset)
                    .build()
                    .user_data(2);
                unsafe {
                    ring.submission().push(&read_e).map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::Other, "Submission queue full")
                    })?;
                }
                ring.submit_and_wait(1)?;

                let mut cq = ring.completion();
                cq.sync();
                let mut read_res = Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "io_uring read CQE not found",
                ));
                for cqe in cq {
                    if cqe.user_data() == 2 {
                        let res = cqe.result();
                        read_res = if res < 0 {
                            Err(std::io::Error::from_raw_os_error(-res))
                        } else {
                            Ok(())
                        };
                        break;
                    }
                }
                read_res
            });

            match res {
                Ok(_) => {
                    let bytes = bytes::Bytes::from(buffer);
                    let aligned_bytes = bytes.slice(align_offset..align_offset + size);
                    (Ok(aligned_bytes), file)
                }
                Err(e) => (Err(e), file),
            }
        })
        .await
        .map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Thread join error on read_block: {}",
                e
            ))
        })?;
        self.return_file(file);

        let res_val = res?;
        crate::fuse_client::METRICS
            .get_obj
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(res_val)
    }
}
