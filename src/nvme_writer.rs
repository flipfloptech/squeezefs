use crate::error::Result;

pub struct NvmeBlockWriter {
    pub device_path: String,
}

impl NvmeBlockWriter {
    pub fn new(device_path: &str) -> Self {
        Self {
            device_path: device_path.to_string(),
        }
    }

    pub async fn write_block(&self, offset: u64, data: &[u8]) -> Result<()> {
        use std::io::SeekFrom;
        use tokio::fs::OpenOptions;
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};

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

        file.sync_all()
            .await
            .map_err(|e| crate::error::SqueezefsError::Io(e))?;

        Ok(())
    }
}
