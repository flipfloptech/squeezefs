use crate::error::Result;
use std::io::SeekFrom;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub struct NvmeBlockDev {
    pub device_path: String,
}

impl NvmeBlockDev {
    pub fn new(device_path: &str) -> Self {
        Self {
            device_path: device_path.to_string(),
        }
    }

    pub async fn write_block(&self, offset: u64, data: &[u8]) -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .open(&self.device_path)
            .await
            .map_err(|e| crate::error::SqueezefsError::Io(e))?;

        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| crate::error::SqueezefsError::Io(e))?;

        file.write_all(data)
            .await
            .map_err(|e| crate::error::SqueezefsError::Io(e))?;

        file.sync_data()
            .await
            .map_err(|e| crate::error::SqueezefsError::Io(e))?;

        Ok(())
    }

    pub async fn read_block(&self, offset: u64, size: usize) -> Result<Vec<u8>> {
        let mut file = OpenOptions::new()
            .read(true)
            .open(&self.device_path)
            .await
            .map_err(|e| crate::error::SqueezefsError::Io(e))?;

        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| crate::error::SqueezefsError::Io(e))?;

        let mut buf = vec![0u8; size];
        file.read_exact(&mut buf)
            .await
            .map_err(|e| crate::error::SqueezefsError::Io(e))?;

        Ok(buf)
    }
}
