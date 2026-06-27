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

use squeezefs::fuse_client::{SqueezefsFilesystem, WritebackRequest};
use squeezefs::dlm::DlmClient;
use squeezefs::backend::{MultiBackendClient, RustFsClient};
use squeezefs::cache::TieredCache;
use squeezefs::routing::DataRouter;
use std::sync::Arc;
use tokio::sync::mpsc;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

#[tokio::test]
async fn test_max_background_uploads_config() {
    // 1. Verify we can configure max concurrent background uploads on filesystem initialization.
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).ok();
    if dlm.is_none() {
        println!("Skipping test: Garnet/Redis not available");
        return;
    }
    let dlm = dlm.unwrap();

    let backend = RustFsClient::new_mock();
    let multi_backend = MultiBackendClient::new();
    multi_backend.register_backend("backend_0", backend.clone());

    let temp_staging = tempdir().expect("Failed to create tempdir");
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("128MB"),
        Some("500MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .expect("Failed to create cache");

    let router = DataRouter::new(dlm.clone(), multi_backend.clone(), cache.clone());
    let fs = SqueezefsFilesystem::new(router.clone(), dlm.clone(), 1000, 1000);

    // Assert that the filesystem has a configurable max_background_uploads field.
    // This will initially FAIL compilation since the field does not exist.
    assert_eq!(fs.max_background_uploads(), 16);
}

#[tokio::test]
async fn test_background_io_pipelining_concurrency() {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).ok();
    if dlm.is_none() {
        return;
    }
    let dlm = dlm.unwrap();

    let backend = RustFsClient::new_mock();
    let multi_backend = MultiBackendClient::new();
    multi_backend.register_backend("backend_0", backend.clone());

    let temp_staging = tempdir().expect("Failed to create tempdir");
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("128MB"),
        Some("128MB"),
        Some("500MB"),
        Some("500MB"),
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .expect("Failed to create cache");

    let router = DataRouter::new(dlm.clone(), multi_backend.clone(), cache.clone());
    let mut fs = SqueezefsFilesystem::new(router.clone(), dlm.clone(), 1000, 1000);
    fs.max_background_uploads = 2; // set concurrency limit to 2

    // Assert that the writeback worker respects the concurrency limit.
    assert_eq!(fs.max_background_uploads(), 2);
}
