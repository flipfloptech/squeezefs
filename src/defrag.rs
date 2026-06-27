use crate::error::Result;
use crate::block_allocator::BlockAllocator;
use crate::nvme_dev::NvmeBlockDev;
use std::sync::Arc;
use indicatif::{ProgressBar, ProgressStyle};

pub async fn run_defragmentation(
    redis_url: &str,
    fs_name: &str,
    nvme_path: &str,
) -> Result<()> {
    let dlm = crate::dlm::DlmClient::new(redis_url)?;
    let client = Arc::new(dlm.meta_client().clone());
    
    let block_alloc = Arc::new(BlockAllocator::new(client.clone(), fs_name).await?);
    let nvme_dev = Arc::new(NvmeBlockDev::new(nvme_path));
    
    // 1. Calculate fragmentation
    let (highest_block, used_blocks, free_blocks, frag_percent) = block_alloc.calculate_fragmentation().await?;
    println!("Volume Fragmentation: {:.2}% (Used: {}, Free: {}, Highest Block: {})", 
        frag_percent, used_blocks, free_blocks, highest_block);
        
    if free_blocks == 0 {
        println!("No fragmentation detected. Exiting.");
        return Ok(());
    }
    
    // 2. Build reverse block map
    println!("Scanning metadata to build reverse block map...");
    let mut conn = client.get_connection().await?;
    let mut block_to_file: std::collections::BTreeMap<u64, (String, String)> = std::collections::BTreeMap::new();
    
    let mut cursor: u64 = 0;
    loop {
        let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg("squeezefs:block_map:*")
            .arg("COUNT")
            .arg(1000)
            .query_async(&mut conn)
            .await?;
            
        for key in keys {
            let block_map_id = key.strip_prefix("squeezefs:block_map:").unwrap_or(&key).to_string();
            let mappings: std::collections::HashMap<String, String> = redis::cmd("HGETALL")
                .arg(&key)
                .query_async(&mut conn)
                .await?;
                
            for (block_idx_str, offset_str) in mappings {
                if let Ok(offset) = offset_str.parse::<u64>() {
                    block_to_file.insert(offset, (block_map_id.clone(), block_idx_str));
                }
            }
        }
        
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }
    
    // 3. Defragment high blocks
    let chunk_size = 4 * 1024 * 1024;
    let free_set_key = format!("{}:free_blocks", fs_name);
    let all_free: Vec<u64> = redis::cmd("SMEMBERS").arg(&free_set_key).query_async(&mut conn).await?;
    let mut free_holes: Vec<u64> = all_free.into_iter().filter(|&idx| idx < used_blocks).collect();
    free_holes.sort();
    
    if free_holes.is_empty() {
        println!("No low-index holes available for defragmentation.");
        return Ok(());
    }
    
    let pb = ProgressBar::new(free_holes.len() as u64);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} blocks defragmented ({eta})")
        .unwrap()
        .progress_chars("#>-"));
        
    for target_hole_idx in free_holes {
        let (&highest_offset, highest_file_info) = match block_to_file.iter().next_back() {
            Some(entry) => entry,
            None => break,
        };
        let highest_file_info = highest_file_info.clone();
        
        if highest_offset <= used_blocks * chunk_size {
            break; // No more high blocks to move
        }
        
        let target_hole_offset = target_hole_idx * chunk_size;
        
        // Claim this hole
        if block_alloc.allocate_specific_block(target_hole_idx).await.is_ok() {
            // Read from highest, write to hole
            if let Ok(data) = nvme_dev.read_block(highest_offset, chunk_size as usize).await {
                if nvme_dev.write_block(target_hole_offset, &data).await.is_ok() {
                    // Atomically update block_map
                    let (map_id, idx_str) = highest_file_info;
                    let key = format!("squeezefs:block_map:{}", map_id);
                    let _: () = redis::cmd("HSET")
                        .arg(&key)
                        .arg(&idx_str)
                        .arg(target_hole_offset.to_string())
                        .query_async(&mut conn)
                        .await?;
                        
                    // Free the old high block
                    block_alloc.free_block(highest_offset).await?;
                    
                    // Update in-memory map
                    block_to_file.remove(&highest_offset);
                    block_to_file.insert(target_hole_offset, (map_id, idx_str));
                }
            }
        }
        
        pb.inc(1);
    }
    
    pb.finish_with_message("Defragmentation complete");
    Ok(())
}
