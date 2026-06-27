import sys
content = open('src/fuse_client.rs').read()

old_create = """            let inodes_limit_str: Option<String> = con
                .hget("squeezefs:format", "inodes")
                .await
                .map_err(map_err)?;
            let max_inodes = inodes_limit_str
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let max_inodes_val = if max_inodes > 0 { max_inodes } else { u64::MAX };

            // Execute LUA_CREATE_SCRIPT
            // KEYS: [dir_key, parent]
            // ARGV: [name, uid, gid, perm, sec, max_inodes, nsec, shard_count]
            let shard_count = self.dlm.shard_count() as u64;
            let res: Vec<u64> = LUA_CREATE_SCRIPT
                .key(&dir_key)
                .key(parent)
                .arg(&*name_str)
                .arg(req.uid)
                .arg(req.gid)
                .arg(mode as u16 & 0o7777)
                .arg(sec)
                .arg(max_inodes_val)
                .arg(nsec)
                .arg(shard_count)
                .invoke_async(&mut con)
                .await
                .map_err(map_err)?;

            let status = res.first().copied().unwrap_or(1);
            let new_ino = res.get(1).copied().unwrap_or(0);

            match status {
                0 => {}
                1 => return Err(Errno::from(libc::EEXIST)),
                2 => return Err(Errno::from(libc::ENOSPC)),
                _ => return Err(Errno::from(libc::EIO)),
            }"""

new_create = """            let inodes_limit_str: Option<String> = con
                .hget("squeezefs:format", "inodes")
                .await
                .map_err(map_err)?;
            let max_inodes = inodes_limit_str
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let max_inodes_val = if max_inodes > 0 { max_inodes } else { u64::MAX };

            let used: u64 = redis::cmd("GET")
                .arg("squeezefs:used_inodes")
                .query_async(&mut con)
                .await
                .unwrap_or(0);
            
            if used >= max_inodes_val {
                return Err(Errno::from(libc::ENOSPC));
            }

            let shard_count = self.dlm.shard_count() as u64;
            let new_ino: u64 = redis::cmd("INCRBY")
                .arg("squeezefs:inode_counter")
                .arg(shard_count)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;

            let inserted: bool = redis::cmd("HSETNX")
                .arg(&dir_key)
                .arg(&*name_str)
                .arg(new_ino)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;

            if !inserted {
                return Err(Errno::from(libc::EEXIST));
            }

            let attr_key = format!("squeezefs:attr:{}", new_ino);
            let meta_key = format!("metadata:inode_{}", new_ino);
            let parent_attr_key = format!("squeezefs:attr:{}", parent);

            let mut pipe = redis::pipe();
            pipe.atomic();
            pipe.hset_multiple(&attr_key, &[
                ("ino", new_ino.to_string()),
                ("size", "0".to_string()),
                ("blocks", "0".to_string()),
                ("kind", "1".to_string()),
                ("perm", (mode as u16 & 0o7777).to_string()),
                ("nlink", "1".to_string()),
                ("uid", req.uid.to_string()),
                ("gid", req.gid.to_string()),
                ("atime_sec", sec.to_string()),
                ("atime_nsec", nsec.to_string()),
                ("mtime_sec", sec.to_string()),
                ("mtime_nsec", nsec.to_string()),
                ("ctime_sec", sec.to_string()),
                ("ctime_nsec", nsec.to_string()),
            ]);
            pipe.hset_multiple(&meta_key, &[
                ("type", "inline".to_string()),
                ("size", "0".to_string()),
            ]);
            pipe.cmd("INCRBY").arg("squeezefs:used_inodes").arg(1);
            pipe.hset_multiple(&parent_attr_key, &[
                ("mtime_sec", sec.to_string()),
                ("mtime_nsec", nsec.to_string()),
                ("ctime_sec", sec.to_string()),
                ("ctime_nsec", nsec.to_string()),
            ]);

            pipe.query_async::<()>(&mut con).await.map_err(map_err)?;"""

old_mkdir = """            let inodes_limit_str: Option<String> = con
                .hget("squeezefs:format", "inodes")
                .await
                .map_err(map_err)?;
            let max_inodes = inodes_limit_str
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let max_inodes_val = if max_inodes > 0 { max_inodes } else { u64::MAX };

            // Execute LUA_MKDIR_SCRIPT
            // KEYS: [dir_key, parent]
            // ARGV: [name, uid, gid, perm, sec, max_inodes, nsec, shard_count]
            let shard_count = self.dlm.shard_count() as u64;
            let res: Vec<u64> = LUA_MKDIR_SCRIPT
                .key(&dir_key)
                .key(parent)
                .arg(&*name_str)
                .arg(req.uid)
                .arg(req.gid)
                .arg(mode as u16 & 0o7777)
                .arg(sec)
                .arg(max_inodes_val)
                .arg(nsec)
                .arg(shard_count)
                .invoke_async(&mut con)
                .await
                .map_err(map_err)?;

            let status = res.first().copied().unwrap_or(1);
            let new_ino = res.get(1).copied().unwrap_or(0);

            match status {
                0 => {}
                1 => return Err(Errno::from(libc::EEXIST)),
                2 => return Err(Errno::from(libc::ENOSPC)),
                _ => return Err(Errno::from(libc::EIO)),
            }"""

new_mkdir = """            let inodes_limit_str: Option<String> = con
                .hget("squeezefs:format", "inodes")
                .await
                .map_err(map_err)?;
            let max_inodes = inodes_limit_str
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let max_inodes_val = if max_inodes > 0 { max_inodes } else { u64::MAX };

            let used: u64 = redis::cmd("GET")
                .arg("squeezefs:used_inodes")
                .query_async(&mut con)
                .await
                .unwrap_or(0);
            
            if used >= max_inodes_val {
                return Err(Errno::from(libc::ENOSPC));
            }

            let shard_count = self.dlm.shard_count() as u64;
            let new_ino: u64 = redis::cmd("INCRBY")
                .arg("squeezefs:inode_counter")
                .arg(shard_count)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;

            let inserted: bool = redis::cmd("HSETNX")
                .arg(&dir_key)
                .arg(&*name_str)
                .arg(new_ino)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;

            if !inserted {
                return Err(Errno::from(libc::EEXIST));
            }

            let attr_key = format!("squeezefs:attr:{}", new_ino);
            let meta_key = format!("metadata:inode_{}", new_ino);
            let child_dir_key = format!("squeezefs:dir:{}", new_ino);
            let parent_attr_key = format!("squeezefs:attr:{}", parent);

            let mut pipe = redis::pipe();
            pipe.atomic();
            pipe.hset_multiple(&attr_key, &[
                ("ino", new_ino.to_string()),
                ("size", "4096".to_string()),
                ("blocks", "8".to_string()),
                ("kind", "2".to_string()),
                ("perm", (mode as u16 & 0o7777).to_string()),
                ("nlink", "2".to_string()),
                ("uid", req.uid.to_string()),
                ("gid", req.gid.to_string()),
                ("atime_sec", sec.to_string()),
                ("atime_nsec", nsec.to_string()),
                ("mtime_sec", sec.to_string()),
                ("mtime_nsec", nsec.to_string()),
                ("ctime_sec", sec.to_string()),
                ("ctime_nsec", nsec.to_string()),
            ]);
            pipe.hset_multiple(&meta_key, &[
                ("type", "inline".to_string()),
                ("size", "0".to_string()),
            ]);
            pipe.hset_multiple(&child_dir_key, &[
                (".", new_ino.to_string()),
                ("..", parent.to_string()),
            ]);
            pipe.cmd("INCRBY").arg("squeezefs:used_inodes").arg(1);
            pipe.cmd("HINCRBY").arg(&parent_attr_key).arg("nlink").arg(1);
            pipe.hset_multiple(&parent_attr_key, &[
                ("mtime_sec", sec.to_string()),
                ("mtime_nsec", nsec.to_string()),
                ("ctime_sec", sec.to_string()),
                ("ctime_nsec", nsec.to_string()),
            ]);

            pipe.query_async::<()>(&mut con).await.map_err(map_err)?;"""

if old_create in content:
    content = content.replace(old_create, new_create)
    print("Replaced create")
else:
    print("Failed to find create")

if old_mkdir in content:
    content = content.replace(old_mkdir, new_mkdir)
    print("Replaced mkdir")
else:
    print("Failed to find mkdir")

open('src/fuse_client.rs', 'w').write(content)
