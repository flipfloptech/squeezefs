import sys
import re

content = open('src/dlm.rs').read()

# 1. Remove static Scripts
content = re.sub(r'static ACQUIRE_SCRIPT: Lazy<redis::Script> = Lazy::new\(\|\| \{.*?\}\);', '', content, flags=re.DOTALL)
content = re.sub(r'const RENEW_SCRIPT_CODE: &str = r#".*?"#;', '', content, flags=re.DOTALL)
content = re.sub(r'static RELEASE_SCRIPT: Lazy<redis::Script> = Lazy::new\(\|\| \{.*?\}\);', '', content, flags=re.DOTALL)
content = re.sub(r'static DELEGATION_ACQUIRE_SCRIPT: Lazy<redis::Script> = Lazy::new\(\|\| \{.*?\}\);', '', content, flags=re.DOTALL)
content = re.sub(r'static DELEGATION_RELEASE_SCRIPT: Lazy<redis::Script> = Lazy::new\(\|\| \{.*?\}\);', '', content, flags=re.DOTALL)

# 2. Patch ACQUIRE
acquire_old = """        let fencing_token: Option<u64> = ACQUIRE_SCRIPT
            .key(&lock_key)
            .key(&fencing_gen_key)
            .arg(&self.client_id)
            .arg(ttl_ms)
            .invoke_async(&mut con)
            .await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e })?;"""
acquire_new = """        let acquired: Option<String> = redis::cmd("SET")
            .arg(&lock_key)
            .arg(&self.client_id)
            .arg("NX")
            .arg("PX")
            .arg(ttl_ms)
            .query_async(&mut con)
            .await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e })?;
        
        let fencing_token: Option<u64> = if acquired.is_some() {
            Some(redis::cmd("INCR").arg(&fencing_gen_key).query_async(&mut con).await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e })?)
        } else {
            None
        };"""
content = content.replace(acquire_old, acquire_new)

# 3. Patch RELEASE (2 occurrences)
release_old = """                    let _: Result<i32> = RELEASE_SCRIPT
                        .key(&lock_key)
                        .arg(&client_id)
                        .invoke_async(&mut con)
                        .await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e });"""
release_new = """                    let current_holder: Option<String> = redis::cmd("GET").arg(&lock_key).query_async(&mut con).await.unwrap_or(None);
                    if current_holder == Some(client_id.clone()) {
                        let _: () = redis::cmd("DEL").arg(&lock_key).query_async(&mut con).await.unwrap_or(());
                    }"""
content = content.replace(release_old, release_new)

release_old2 = """        let _res: i32 = RELEASE_SCRIPT
            .key(&lock_key)
            .arg(&self.client_id)
            .invoke_async(&mut con)
            .await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e })?;"""
release_new2 = """        let current_holder: Option<String> = redis::cmd("GET").arg(&lock_key).query_async(&mut con).await.unwrap_or(None);
        if current_holder == Some(self.client_id.clone()) {
            let _: () = redis::cmd("DEL").arg(&lock_key).query_async(&mut con).await.unwrap_or(());
        }"""
content = content.replace(release_old2, release_new2)

# 4. Patch DELEGATION_ACQUIRE
delegation_acquire_old = """        let holder: String = DELEGATION_ACQUIRE_SCRIPT
            .key(&delegation_key)
            .arg(&self.client_id)
            .arg(ttl_ms)
            .invoke_async(&mut con)
            .await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e })?;"""
delegation_acquire_new = """        let current_holder: Option<String> = redis::cmd("GET").arg(&delegation_key).query_async(&mut con).await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e })?;
        let holder = if let Some(h) = current_holder {
            h
        } else {
            let _: () = redis::cmd("SET").arg(&delegation_key).arg(&self.client_id).arg("PX").arg(ttl_ms).query_async(&mut con).await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e })?;
            self.client_id.clone()
        };"""
content = content.replace(delegation_acquire_old, delegation_acquire_new)

# 5. Patch DELEGATION_RELEASE (2 occurrences)
del_rel_old = """                    let _: Result<i32> = DELEGATION_RELEASE_SCRIPT
                        .key(&lock_key)
                        .arg(&client_id)
                        .invoke_async(&mut con)
                        .await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e });"""
del_rel_new = """                    let current_holder: Option<String> = redis::cmd("GET").arg(&lock_key).query_async(&mut con).await.unwrap_or(None);
                    if current_holder == Some(client_id.clone()) {
                        let _: () = redis::cmd("DEL").arg(&lock_key).query_async(&mut con).await.unwrap_or(());
                    }"""
content = content.replace(del_rel_old, del_rel_new)

del_rel_old2 = """        let _: i32 = DELEGATION_RELEASE_SCRIPT
            .key(&delegation_key)
            .arg(&self.client_id)
            .invoke_async(&mut con)
            .await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e })?;"""
del_rel_new2 = """        let current_holder: Option<String> = redis::cmd("GET").arg(&delegation_key).query_async(&mut con).await.unwrap_or(None);
        if current_holder == Some(self.client_id.clone()) {
            let _: () = redis::cmd("DEL").arg(&delegation_key).query_async(&mut con).await.unwrap_or(());
        }"""
content = content.replace(del_rel_old2, del_rel_new2)

# 6. Patch RENEW loop
renew_old = """                let mut pipe = redis::pipe();
                for key in &keys_to_renew {
                    if let Some(lease) = leases.get(key) {
                        pipe.cmd("EVAL")
                            .arg(RENEW_SCRIPT_CODE)
                            .arg(1)
                            .arg(&lease.lock_key)
                            .arg(&lease.client_id)
                            .arg(lease.ttl_ms);
                    }
                }

                match pipe.query_async::<_, Vec<i32>>(&mut con).await {
                    Ok(results) => {
                        con_opt = Some(con);
                        for (i, key) in keys_to_renew.into_iter().enumerate() {
                            if let Some(lease) = leases.get_mut(&key) {
                                let res_val = results.get(i).copied().unwrap_or(0);
                                if res_val == 1 {
                                    debug!("Heartbeat manager: Successfully renewed lease for key: {}", key);
                                    let interval_dur = Duration::from_millis(lease.ttl_ms / 3);
                                    lease.next_renewal = Instant::now() + interval_dur;
                                } else {
                                    error!("Heartbeat manager: Failed to renew lease for key: {} - lock stolen or expired", key);
                                    leases.remove(&key);
                                }
                            }
                        }
                    }"""
renew_new = """                let mut pipe_get = redis::pipe();
                for key in &keys_to_renew {
                    pipe_get.cmd("GET").arg(key);
                }

                match pipe_get.query_async::<Vec<Option<String>>>(&mut con).await {
                    Ok(holders) => {
                        let mut pipe_renew = redis::pipe();
                        let mut renewals = Vec::new();
                        for (i, key) in keys_to_renew.iter().enumerate() {
                            if let Some(lease) = leases.get(key) {
                                if let Some(Some(holder)) = holders.get(i) {
                                    if holder == &lease.client_id {
                                        pipe_renew.cmd("PEXPIRE").arg(key).arg(lease.ttl_ms);
                                        renewals.push((key.clone(), true));
                                        continue;
                                    }
                                }
                                renewals.push((key.clone(), false));
                            }
                        }
                        if !renewals.is_empty() {
                            let _: () = pipe_renew.query_async(&mut con).await.unwrap_or(());
                        }
                        con_opt = Some(con);
                        for (key, success) in renewals {
                            if success {
                                if let Some(mut lease) = leases.get_mut(&key) {
                                    debug!("Heartbeat manager: Successfully renewed lease for key: {}", key);
                                    let interval_dur = Duration::from_millis(lease.ttl_ms / 3);
                                    lease.next_renewal = Instant::now() + interval_dur;
                                }
                            } else {
                                error!("Heartbeat manager: Failed to renew lease for key: {} - lock stolen or expired", key);
                                leases.remove(&key);
                            }
                        }
                    }"""
content = content.replace(renew_old, renew_new)

open('src/dlm.rs', 'w').write(content)
print("Finished patching dlm.rs")
