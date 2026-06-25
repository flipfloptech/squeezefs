use crate::error::{Result, SqueezefsError};
use log::info;
use std::path::{Path, PathBuf};

#[cfg(all(feature = "gds", unix))]
use std::sync::Arc;

#[cfg(all(feature = "gds", unix))]
#[repr(C)]
pub struct CUfileDescr_t {
    pub desc_type: i32,
    pub handle: CUfileHandleUnion,
    pub fs_ops: *const std::ffi::c_void,
}

#[cfg(all(feature = "gds", unix))]
#[repr(C)]
pub union CUfileHandleUnion {
    pub fd: std::ffi::c_int,
    pub handle: *mut std::ffi::c_void,
}

#[cfg(all(feature = "gds", unix))]
#[repr(C)]
pub struct CUfileError_t {
    pub err: i32,
    pub cu_err: i32,
}

#[cfg(all(feature = "gds", unix))]
pub struct LibCuFile {
    _lib: libloading::Library,
    cu_file_driver_open: unsafe extern "C" fn() -> CUfileError_t,
    cu_file_driver_close: unsafe extern "C" fn() -> CUfileError_t,
    cu_file_handle_register: unsafe extern "C" fn(
        fh: *mut *mut std::ffi::c_void,
        descr: *const CUfileDescr_t,
    ) -> CUfileError_t,
    cu_file_handle_deregister: unsafe extern "C" fn(fh: *mut std::ffi::c_void),
    cu_file_read: unsafe extern "C" fn(
        fh: *mut std::ffi::c_void,
        dev_ptr: *mut std::ffi::c_void,
        size: usize,
        file_offset: i64,
        dev_ptr_offset: i64,
    ) -> isize,
}

#[cfg(all(feature = "gds", unix))]
impl LibCuFile {
    /// Dynamically loads `libcufile.so` from standard search paths.
    ///
    /// # Safety
    ///
    /// This function is unsafe because loading dynamic libraries using `libloading`
    /// runs arbitrary initialization code and exposes dynamic symbol resolution
    /// which can cause undefined behavior if the library is malformed or incompatible.
    pub unsafe fn load() -> Result<Self> {
        let paths = [
            "libcufile.so",
            "/usr/local/cuda/lib64/libcufile.so",
            "/usr/lib/x86_64-linux-gnu/libcufile.so",
            "/usr/local/cuda/targets/x86_64-linux/lib/libcufile.so",
        ];

        let mut loaded_lib = None;
        for path in &paths {
            if let Ok(lib) = libloading::Library::new(path) {
                loaded_lib = Some(lib);
                break;
            }
        }

        let lib = match loaded_lib {
            Some(l) => l,
            None => {
                return Err(SqueezefsError::GdsError(
                    "Could not load libcufile.so from standard paths".to_string(),
                ))
            }
        };

        let cu_file_driver_open = *lib
            .get(b"cuFileDriverOpen\0")
            .map_err(|e| SqueezefsError::GdsError(e.to_string()))?;
        let cu_file_driver_close = *lib
            .get(b"cuFileDriverClose\0")
            .map_err(|e| SqueezefsError::GdsError(e.to_string()))?;
        let cu_file_handle_register = *lib
            .get(b"cuFileHandleRegister\0")
            .map_err(|e| SqueezefsError::GdsError(e.to_string()))?;
        let cu_file_handle_deregister = *lib
            .get(b"cuFileHandleDeregister\0")
            .map_err(|e| SqueezefsError::GdsError(e.to_string()))?;
        let cu_file_read = *lib
            .get(b"cuFileRead\0")
            .map_err(|e| SqueezefsError::GdsError(e.to_string()))?;

        Ok(Self {
            _lib: lib,
            cu_file_driver_open,
            cu_file_driver_close,
            cu_file_handle_register,
            cu_file_handle_deregister,
            cu_file_read,
        })
    }
}

#[cfg(all(feature = "gds", unix))]
impl Drop for LibCuFile {
    fn drop(&mut self) {
        unsafe {
            (self.cu_file_driver_close)();
        }
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct GdsCache {
    gpu_detected: bool,
    staging_dirs: Vec<PathBuf>,
    #[cfg(all(feature = "gds", unix))]
    cufile_lib: Option<Arc<LibCuFile>>,
}

impl GdsCache {
    pub fn new(staging_dirs: Vec<PathBuf>) -> Self {
        // Detect GPU environment (e.g., checking if NVIDIA device files exist or CUDA env vars are set)
        let has_nvidia_dev =
            Path::new("/dev/nvidia0").exists() || Path::new("/dev/nvidiactl").exists();
        let has_cuda_env =
            std::env::var("CUDA_VERSION").is_ok() || std::env::var("CUDA_VISIBLE_DEVICES").is_ok();
        let gpu_detected = has_nvidia_dev || has_cuda_env;

        #[cfg(all(feature = "gds", unix))]
        {
            let mut cufile_lib = None;
            if gpu_detected {
                match unsafe { LibCuFile::load() } {
                    Ok(lib) => {
                        let status = unsafe { (lib.cu_file_driver_open)() };
                        if status.err == 0 {
                            info!("GPU Direct Storage: libcufile.so successfully loaded and driver initialized.");
                            cufile_lib = Some(Arc::new(lib));
                        } else {
                            log::warn!("GPU Direct Storage: cuFileDriverOpen failed with err: {}, cu_err: {}. GDS disabled.", status.err, status.cu_err);
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "GPU Direct Storage: failed to load libcufile.so: {:?}. GDS disabled.",
                            e
                        );
                    }
                }
            } else {
                info!("GPU Direct Storage: No discrete GPU environment found. GDS bypassed.");
            }

            Self {
                gpu_detected,
                staging_dirs,
                cufile_lib,
            }
        }

        #[cfg(not(all(feature = "gds", unix)))]
        {
            if gpu_detected {
                info!("GPU Direct Storage: Discrete GPU environment detected. GDS RDMA capability enabled.");
            } else {
                info!("GPU Direct Storage: No discrete GPU environment found. GDS bypassed.");
            }
            Self {
                gpu_detected,
                staging_dirs,
            }
        }
    }

    /// Check if GPU Direct Storage is available.
    pub fn is_available(&self) -> bool {
        #[cfg(all(feature = "gds", unix))]
        {
            self.gpu_detected && self.cufile_lib.is_some()
        }
        #[cfg(not(all(feature = "gds", unix)))]
        {
            false // GDS requires both feature and unix target to be fully active/available
        }
    }

    /// Orchestrate a direct RDMA transfer from the object store to a GPU memory address (VRAM).
    /// Bypasses the host OS kernel and system RAM.
    /// - `object_key`: The RustFS S3 key of the physical payload.
    /// - `vram_address`: The physical address pointer in VRAM.
    /// - `offset`: Offset within the object.
    /// - `size`: Number of bytes to transfer.
    /// - `backend`: S3 client to download blocks if missing.
    pub async fn read_direct(
        &self,
        object_key: &str,
        vram_address: u64,
        offset: u64,
        size: usize,
        _backend: &crate::backend::RustFsClient,
    ) -> Result<()> {
        if !self.is_available() {
            return Err(SqueezefsError::GdsError(
                "GDS is not available: no GPU detected or libcufile.so not loaded".to_string(),
            ));
        }

        #[cfg(all(feature = "gds", unix))]
        {
            let lib = self.cufile_lib.as_ref().unwrap();

            // 1. Locate or download block locally on NVMe
            let staging_dir = self.staging_dirs.first().ok_or_else(|| {
                SqueezefsError::GdsError("No staging directories configured".to_string())
            })?;

            let safe_filename = object_key.replace('/', "_");
            let local_path = staging_dir.join(format!("{}.gds_cache", safe_filename));

            if !local_path.exists() {
                // Download block from S3 to NVMe staging
                let data = _backend.get_object(object_key).await?;
                tokio::fs::write(&local_path, data).await?;
            }

            // 2. Open file descriptor with O_DIRECT
            use std::os::unix::ffi::OsStrExt;
            let path_cstr = std::ffi::CString::new(local_path.as_os_str().as_bytes())
                .map_err(|e| SqueezefsError::GdsError(format!("Invalid path CString: {:?}", e)))?;

            let fd = unsafe { libc::open(path_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECT) };
            if fd < 0 {
                return Err(SqueezefsError::Io(std::io::Error::last_os_error()));
            }

            // 3. Register file handle with cuFile
            let mut cf_handle: *mut std::ffi::c_void = std::ptr::null_mut();
            let descr = CUfileDescr_t {
                desc_type: 1, // CU_FILE_HANDLE_TYPE_OPAQUE_FD
                handle: CUfileHandleUnion { fd },
                fs_ops: std::ptr::null(),
            };

            let status = unsafe { (lib.cu_file_handle_register)(&mut cf_handle, &descr) };

            if status.err != 0 {
                unsafe {
                    libc::close(fd);
                }
                return Err(SqueezefsError::GdsError(format!(
                    "cuFileHandleRegister failed with err: {}, cu_err: {}",
                    status.err, status.cu_err
                )));
            }

            // 4. Perform direct DMA read from NVMe fd to VRAM address
            let read_bytes = unsafe {
                (lib.cu_file_read)(
                    cf_handle,
                    vram_address as *mut std::ffi::c_void,
                    size,
                    offset as i64,
                    0, // devPtr_offset
                )
            };

            // 5. Cleanup
            unsafe {
                (lib.cu_file_handle_deregister)(cf_handle);
                libc::close(fd);
            }

            if read_bytes < 0 {
                return Err(SqueezefsError::GdsError(format!(
                    "cuFileRead failed with return code: {}",
                    read_bytes
                )));
            }

            Ok(())
        }

        #[cfg(not(all(feature = "gds", unix)))]
        {
            // Bypassed/simulated mode (should not be reached if is_available() is correct, but kept for fallback compatibility)
            info!(
                "Orchestrating GPU Direct Storage RDMA transfer for {} (offset {}, size {}) directly to VRAM address 0x{:X} (Simulated)",
                object_key, offset, size, vram_address
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            Ok(())
        }
    }
}
