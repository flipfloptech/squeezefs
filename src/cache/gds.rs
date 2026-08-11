use crate::error::{Result, SqueezefsError};
use log::{info, warn};
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
    pub force_available: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// True once ANY `.gds_cache` file was materialized this mount
    /// (write-IOPS economy, 2026-08-11): the per-op purge fires an
    /// `unlink(2)` + two path allocations per block invalidation, and on
    /// the (default) GDS-idle mount every one is ENOENT ceremony. Within
    /// a mount every producer sets this before its file exists, so a
    /// false-`false` skip is unrepresentable; cross-mount staleness is
    /// owned by `wipe_gds_cache_files` exactly as before.
    materialized: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
                force_available: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                materialized: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
                force_available: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                materialized: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
    }

    /// Check if GPU Direct Storage is available.
    /// Arm the purge gate: a producer OUTSIDE this module (the routing
    /// prefetch arm) is about to materialize a `.gds_cache` file. Must be
    /// called BEFORE the file exists (the gate's ordering law).
    pub fn note_materialized(&self) {
        self.materialized
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_available(&self) -> bool {
        if self
            .force_available
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return true;
        }

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
    /// - `object_key`: The backing block index/offset of the physical payload.
    /// - `vram_address`: The physical address pointer in VRAM.
    /// - `offset`: Offset within the object.
    /// - `size`: Number of bytes to transfer.
    /// - `router`: DataRouter to load/fetch blocks if missing.
    pub async fn read_direct(
        &self,
        object_key: &str,
        vram_address: u64,
        offset: u64,
        size: usize,
        router: &crate::routing::DataRouter,
    ) -> Result<()> {
        if !self.is_available() {
            return Err(SqueezefsError::GdsError(
                "GDS is not available: no GPU detected or libcufile.so not loaded".to_string(),
            ));
        }

        #[cfg(all(feature = "gds", unix))]
        {
            let lib = self.cufile_lib.as_ref().unwrap();

            // 1. Locate or download block locally on NVMe.
            // One construction function for `.gds_cache` names (PR 3, R4
            // §5.4): the historical inline sanitizer here diverged from
            // `get_gds_path` on PREFIXED keys (`be_id://offset` →
            // `be_id:__offset` vs `be_id___offset`), so a purge of the
            // canonical name missed the copy this path wrote AND served.
            // Pre-unification files under the old scheme need no
            // migration: the mount-time `wipe_gds_cache_files` sweep
            // matches the `.gds_cache` suffix and catches both.
            let local_path = self.get_gds_path(object_key).ok_or_else(|| {
                SqueezefsError::GdsError("No staging directories configured".to_string())
            })?;

            if !local_path.exists() {
                // Fetch the block using the router (handles decompression, decryption, and caching layers)
                let data = router.get_cached_or_fetch_block(object_key).await?;
                // The purge gate arms BEFORE the file exists (see
                // `materialized`), so a concurrent invalidation can
                // never skip a file this write is about to create.
                self.materialized
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                // P2-8: GDS local cache materialization via process io_uring file worker.
                crate::uring_fs::write_all(&local_path, data.to_vec()).await?;
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
            // Fetch block to simulate the read flow and verify caching/backend access
            let _data = router.get_cached_or_fetch_block(object_key).await?;
            info!(
                "Orchestrating GPU Direct Storage RDMA transfer for {} (offset {}, size {}) directly to VRAM address 0x{:X} (Simulated)",
                object_key, offset, size, vram_address
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            Ok(())
        }
    }

    /// Drop the `.gds_cache` file for a freed/displaced block key —
    /// called ONLY from `TieredCache::purge_block_key` (the unified
    /// four-tier purge, §5.4). Unlink-if-exists; `ENOENT` ignored (most
    /// keys never had a GDS copy). Complete by construction: every
    /// producer builds its name through `get_gds_path` (PR 3 unification).
    pub fn remove_cached(&self, object_key: &str) {
        // GDS-idle mounts (the default fleet) skip the per-invalidation
        // path mint + unlink ceremony entirely — see `materialized`.
        if !self.materialized.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if let Some(path) = self.get_gds_path(object_key) {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(
                        "GDS cache purge of {} failed (stale file may persist \
                         until the next mount-time wipe): {e}",
                        path.display()
                    );
                }
            }
        }
    }

    /// Helper to resolve the `.gds_cache` filepath for GDS block prefetching.
    pub fn get_gds_path(&self, object_key: &str) -> Option<PathBuf> {
        let staging_dir = self.staging_dirs.first()?;
        let safe_filename = object_key.replace('/', "_");
        Some(staging_dir.join(format!("{}.gds_cache", safe_filename)))
    }
}
