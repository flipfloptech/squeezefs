import sys

content = open('src/fuse_client.rs').read()

old_unlink = """            // Execute LUA_UNLINK_SCRIPT
            // KEYS: [dir_key, parent]
            // ARGV: [name, sec, nsec]
            let res: Vec<i64> = LUA_UNLINK_SCRIPT
                .key(&dir_key)
                .key(parent)
                .arg(&*name_str)
                .arg(sec)
                .arg(nsec)
                .invoke_async(&mut con)
                .await
                .map_err(map_err)?;

            let status = res.first().copied().unwrap_or(-1);
            let ino = res.get(1).copied().unwrap_or(0) as u64;
            let new_nlink = res.get(2).copied().unwrap_or(0);
            let file_size = res.get(3).copied().unwrap_or(0) as u64;

            match status {
                -1 => return Err(Errno::from(libc::ENOENT)),
                1 => return Err(Errno::from(libc::EISDIR)),
                0 => {}
                _ => return Err(Errno::from(libc::EIO)),
            }"""

new_unlink = """            let ino_str: Option<String> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
            let ino = match ino_str {
                Some(s) => s.parse::<u64>().unwrap_or(0),
                None => return Err(Errno::from(libc::ENOENT)),
            };

            let child_attr_key = format!("squeezefs:attr:{}", ino);
            let kind: u32 = con.hget(&child_attr_key, "kind").await.unwrap_or(1);
            if kind == 2 {
                return Err(Errno::from(libc::EISDIR));
            }

            let deleted: i64 = redis::cmd("HDEL").arg(&dir_key).arg(&*name_str).query_async(&mut con).await.map_err(map_err)?;
            if deleted == 0 {
                return Err(Errno::from(libc::ENOENT));
            }

            let mut new_nlink: i64 = redis::cmd("HINCRBY").arg(&child_attr_key).arg("nlink").arg(-1).query_async(&mut con).await.map_err(map_err)?;
            if new_nlink < 0 {
                new_nlink = 0;
                let _: () = redis::cmd("HSET").arg(&child_attr_key).arg("nlink").arg(0).query_async(&mut con).await.map_err(map_err)?;
            }
            
            let file_size: u64 = con.hget(&child_attr_key, "size").await.unwrap_or(0);
            
            let parent_attr_key = format!("squeezefs:attr:{}", parent);
            let _: () = redis::cmd("HSET").arg(&parent_attr_key).arg("mtime_sec").arg(sec).arg("mtime_nsec").arg(nsec).arg("ctime_sec").arg(sec).arg("ctime_nsec").arg(nsec).query_async(&mut con).await.map_err(map_err)?;"""


old_rmdir = """            // Execute LUA_RMDIR_SCRIPT
            // KEYS: [dir_key, parent]
            // ARGV: [name, sec, nsec]
            let res: Vec<u64> = LUA_RMDIR_SCRIPT
                .key(&dir_key)
                .key(parent)
                .arg(&*name_str)
                .arg(sec)
                .arg(nsec)
                .invoke_async(&mut con)
                .await
                .map_err(map_err)?;

            let status = res.first().copied().unwrap_or(1);
            let ino = res.get(1).copied().unwrap_or(0);

            match status {
                0 => {}
                1 => return Err(Errno::from(libc::ENOENT)),
                2 => return Err(Errno::from(libc::ENOTEMPTY)),
                _ => return Err(Errno::from(libc::EIO)),
            }"""

new_rmdir = """            let ino_str: Option<String> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
            let ino = match ino_str {
                Some(s) => s.parse::<u64>().unwrap_or(0),
                None => return Err(Errno::from(libc::ENOENT)),
            };

            let child_dir_key = format!("squeezefs:dir:{}", ino);
            
            loop {
                let _: () = redis::cmd("WATCH").arg(&child_dir_key).query_async(&mut con).await.map_err(map_err)?;
                
                let size: u64 = redis::cmd("HLEN").arg(&child_dir_key).query_async(&mut con).await.map_err(map_err)?;
                if size > 2 {
                    let _: () = redis::cmd("UNWATCH").query_async(&mut con).await.map_err(map_err)?;
                    return Err(Errno::from(libc::ENOTEMPTY));
                }

                let mut pipe = redis::pipe();
                pipe.atomic();
                pipe.cmd("HDEL").arg(&dir_key).arg(&*name_str);
                pipe.cmd("DEL").arg(&child_dir_key);
                
                let res: Option<Vec<i64>> = pipe.query_async(&mut con).await.map_err(map_err)?;
                if res.is_some() {
                    break;
                }
            }

            let child_attr_key = format!("squeezefs:attr:{}", ino);
            let child_meta_key = format!("metadata:inode_{}", ino);
            let child_inline_key = format!("inline_data:inode_{}", ino);
            
            let mut pipe = redis::pipe();
            pipe.cmd("DEL").arg(&child_attr_key);
            pipe.cmd("DEL").arg(&child_meta_key);
            pipe.cmd("DEL").arg(&child_inline_key);
            pipe.cmd("DECR").arg("squeezefs:used_inodes");
            pipe.query_async::<()>(&mut con).await.map_err(map_err)?;
            
            let parent_attr_key = format!("squeezefs:attr:{}", parent);
            let parent_nlink: i64 = con.hget(&parent_attr_key, "nlink").await.unwrap_or(2);
            let mut new_nlink = parent_nlink - 1;
            if new_nlink < 1 {
                new_nlink = 1;
            }
            let _: () = redis::cmd("HSET").arg(&parent_attr_key).arg("nlink").arg(new_nlink).arg("mtime_sec").arg(sec).arg("mtime_nsec").arg(nsec).arg("ctime_sec").arg(sec).arg("ctime_nsec").arg(nsec).query_async(&mut con).await.map_err(map_err)?;"""

if old_unlink in content:
    content = content.replace(old_unlink, new_unlink)
    print("Replaced unlink")
else:
    print("Failed to find unlink")

if old_rmdir in content:
    content = content.replace(old_rmdir, new_rmdir)
    print("Replaced rmdir")
else:
    print("Failed to find rmdir")

open('src/fuse_client.rs', 'w').write(content)

