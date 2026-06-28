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
use once_cell::sync::OnceCell;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::sync::Arc;

pub static SIMULATE_CORRUPTION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_simulate_corruption(val: bool) {
    SIMULATE_CORRUPTION.store(val, std::sync::atomic::Ordering::Relaxed);
}

pub struct NvmeBlockDev {
    pub device_path: String,
    file: OnceCell<Arc<File>>,
}

impl NvmeBlockDev {
    pub fn new(device_path: &str) -> Self {
        Self {
            device_path: device_path.to_string(),
            file: OnceCell::new(),
        }
    }

    fn get_file(&self) -> Result<Arc<File>> {
        self.file
            .get_or_try_init(|| {
                let std_file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.device_path)
                    .map_err(|e| crate::error::SqueezefsError::Io(e))?;
                Ok(Arc::new(std_file))
            })
            .cloned()
    }

    pub async fn write_block(&self, offset: u64, data: &[u8]) -> Result<()> {
        let file = self.get_file()?;
        let data_vec = data.to_vec();
        tokio::task::spawn_blocking(move || {
            file.write_all_at(&data_vec, offset)
                .map_err(|e| crate::error::SqueezefsError::Io(e))?;
            file.sync_data()
                .map_err(|e| crate::error::SqueezefsError::Io(e))?;
            Ok::<(), crate::error::SqueezefsError>(())
        })
        .await
        .map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Thread join error on write_block: {}",
                e
            ))
        })??;
        crate::fuse_client::METRICS
            .put_obj
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        if crate::write_verification_enabled() {
            let mut read_data = self.read_block(offset, data.len()).await?;
            if SIMULATE_CORRUPTION.load(std::sync::atomic::Ordering::Relaxed) {
                if !read_data.is_empty() {
                    read_data[0] ^= 0xFF;
                }
            }
            if read_data != data {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Write verification failed: checksum mismatch at offset {} on device {}",
                    offset, self.device_path
                )));
            }
        }

        Ok(())
    }

    pub async fn read_block(&self, offset: u64, size: usize) -> Result<Vec<u8>> {
        let file = self.get_file()?;
        let res = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; size];
            file.read_exact_at(&mut buf, offset)
                .map_err(|e| crate::error::SqueezefsError::Io(e))?;
            Ok::<Vec<u8>, crate::error::SqueezefsError>(buf)
        })
        .await
        .map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Thread join error on read_block: {}",
                e
            ))
        })??;
        crate::fuse_client::METRICS
            .get_obj
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(res)
    }
}
