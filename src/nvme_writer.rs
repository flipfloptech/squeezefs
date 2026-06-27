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

    pub async fn write_block(&self, _offset: u64, _data: &[u8]) -> Result<()> {
        unimplemented!("TDD: not yet implemented")
    }
}
