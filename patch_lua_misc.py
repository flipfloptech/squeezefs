import sys

content_fuse = open('src/fuse_client.rs').read()
old_write_lua = """            let lua_script = r#"
                local current = redis.call('HGET', KEYS[1], 'size')
                if not current then current = 0 end
                local new_size = tonumber(ARGV[1])
                local diff = 0
                if new_size > tonumber(current) then
                    diff = new_size - tonumber(current)
                    redis.call('HSET', KEYS[1], 'size', new_size)
                    redis.call('HSET', KEYS[1], 'mtime_sec', ARGV[2])
                    redis.call('HSET', KEYS[1], 'mtime_nsec', ARGV[3])
                    redis.call('HSET', KEYS[1], 'ctime_sec', ARGV[2])
                    redis.call('HSET', KEYS[1], 'ctime_nsec', ARGV[3])
                    redis.call('HSET', KEYS[2], 'size', new_size)
                    if ARGV[4] ~= "0" then
                        redis.call('HSET', KEYS[2], 'num_blocks', ARGV[4])
                    end
                else
                    redis.call('HSET', KEYS[1], 'mtime_sec', ARGV[2])
                    redis.call('HSET', KEYS[1], 'mtime_nsec', ARGV[3])
                    redis.call('HSET', KEYS[1], 'ctime_sec', ARGV[2])
                    redis.call('HSET', KEYS[1], 'ctime_nsec', ARGV[3])
                    new_size = tonumber(current)
                end
                if diff > 0 then
                    redis.call('INCRBY', 'squeezefs:used_bytes', diff)
                end
                return new_size
            "#;

            let actual_new_size: u64 = redis::cmd("EVAL")
                .arg(lua_script)
                .arg(2)
                .arg(&attr_key)
                .arg(&meta_key)
                .arg(new_size)
                .arg(sec)
                .arg(nsec)
                .arg(num_blocks)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;"""

new_write_lua = """            let current_str: Option<String> = con.hget(&attr_key, "size").await.map_err(map_err)?;
            let current = current_str.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let mut diff = 0;
            let mut actual_new_size = current;
            
            let mut pipe = redis::pipe();
            pipe.atomic();

            if new_size > current {
                diff = new_size - current;
                actual_new_size = new_size;
                pipe.hset_multiple(&attr_key, &[
                    ("size", new_size.to_string()),
                    ("mtime_sec", sec.to_string()),
                    ("mtime_nsec", nsec.to_string()),
                    ("ctime_sec", sec.to_string()),
                    ("ctime_nsec", nsec.to_string()),
                ]);
                pipe.cmd("HSET").arg(&meta_key).arg("size").arg(new_size.to_string());
                if num_blocks > 0 {
                    pipe.cmd("HSET").arg(&meta_key).arg("num_blocks").arg(num_blocks.to_string());
                }
            } else {
                pipe.hset_multiple(&attr_key, &[
                    ("mtime_sec", sec.to_string()),
                    ("mtime_nsec", nsec.to_string()),
                    ("ctime_sec", sec.to_string()),
                    ("ctime_nsec", nsec.to_string()),
                ]);
            }
            if diff > 0 {
                pipe.cmd("INCRBY").arg("squeezefs:used_bytes").arg(diff);
            }
            let _: () = pipe.query_async(&mut con).await.map_err(map_err)?;"""

if old_write_lua in content_fuse:
    content_fuse = content_fuse.replace(old_write_lua, new_write_lua)
    open('src/fuse_client.rs', 'w').write(content_fuse)
    print("Replaced lua in fuse_client.rs write")
else:
    print("Failed to replace lua in fuse_client.rs write")

