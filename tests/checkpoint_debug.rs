use std::path::PathBuf;
use squeezefs::backend::{MultiBackendClient, RustFsClient};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{format_volume, SqueezefsFilesystem};
use squeezefs::routing::DataRouter;
use tempfile::tempdir;
use fuse3::raw::Request;
use redis::AsyncCommands;

#[tokio::test]
async fn test_debug() {
    let redis_url = "redis://127.0.0.1:6379/15".to_string();
    let mut con = redis::Client::open(redis_url.clone()).unwrap().get_multiplexed_tokio_connection().await.unwrap();
    let _: () = redis::cmd("FLUSHDB").query_async(&mut con).await.unwrap();
    
    let fs_name = "checkpoint_test_vol";
    format_volume(&redis_url, fs_name, 1024 * 1024, 100 * 1024 * 1024 * 1024, 0, "none", "none", None, Some("128MB"), Some("500MB"), Some(&[PathBuf::from("/tmp/squeezefs_staging_checkpoint")]), None, None, None, None, None, None, None, None).await.unwrap();

    let dlm = DlmClient::new(&redis_url).unwrap();
    let mut dlm_con = dlm.get_connection().await.unwrap();
    
    let dir_key = "squeezefs:dir:1";
    let script = redis::Script::new(r#"
        if redis.call("HEXISTS", KEYS[1], ARGV[1]) == 1 then return {1, 0} end
        local used = tonumber(redis.call("GET", "squeezefs:used_inodes") or "0")
        if used >= tonumber(ARGV[6]) then return {2, 0} end
        local new_ino = redis.call("INCRBY", "squeezefs:inode_counter", ARGV[8])
        if redis.call("HSETNX", KEYS[1], ARGV[1], new_ino) == 0 then return {1, 0} end
        local attr_key = "squeezefs:attr:" .. new_ino
        local meta_key = "metadata:inode_" .. new_ino
        redis.call("HSET", attr_key, "ino", new_ino, "size", "0", "blocks", "0", "kind", "1", "perm", ARGV[4], "nlink", "1", "uid", ARGV[2], "gid", ARGV[3], "atime_sec", ARGV[5], "atime_nsec", ARGV[7], "mtime_sec", ARGV[5], "mtime_nsec", ARGV[7], "ctime_sec", ARGV[5], "ctime_nsec", ARGV[7])
        redis.call("HSET", meta_key, "type", "inline", "size", "0")
        redis.call("INCRBY", "squeezefs:used_inodes", 1)
        local parent_attr_key = "squeezefs:attr:" .. KEYS[2]
        redis.call("HSET", parent_attr_key, "mtime_sec", ARGV[5], "mtime_nsec", ARGV[7], "ctime_sec", ARGV[5], "ctime_nsec", ARGV[7])
        return {0, new_ino}
    "#);
    
    let res: Result<Vec<u64>, _> = script.key(dir_key).key(1).arg("checkpoint.bin").arg(1000).arg(1000).arg(0o644).arg(0).arg(0).arg(0).arg(0).invoke_async(&mut dlm_con).await;
    println!("LUA SCRIPT RESULT: {:?}", res);
}
