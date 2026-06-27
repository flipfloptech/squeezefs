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
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use once_cell::sync::OnceCell;

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
        self.file.get_or_try_init(|| {
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
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || {
            file.write_all_at(&data, offset)
                .map_err(|e| crate::error::SqueezefsError::Io(e))?;
            file.sync_data()
                .map_err(|e| crate::error::SqueezefsError::Io(e))?;
            Ok(())
        })
        .await
        .map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Thread join error on write_block: {}",
                e
            ))
        })?
    }

    pub async fn read_block(&self, offset: u64, size: usize) -> Result<Vec<u8>> {
        let file = self.get_file()?;
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; size];
            file.read_exact_at(&mut buf, offset)
                .map_err(|e| crate::error::SqueezefsError::Io(e))?;
            Ok(buf)
        })
        .await
        .map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Thread join error on read_block: {}",
                e
            ))
        })?
    }
}
