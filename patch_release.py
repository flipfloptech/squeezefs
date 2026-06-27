import sys
import re

content = open('src/dlm.rs').read()

# Replace RELEASE_SCRIPT block 1
content = re.sub(
    r'let _res: i32 = RELEASE_SCRIPT\s*\.key\(&self\.lock_key\)\s*\.arg\(&self\.client_id\)\s*\.invoke_async\(&mut con\)\s*\.await[^;]*;',
    r'''let current_holder: Option<String> = redis::cmd("GET").arg(&self.lock_key).query_async(&mut con).await.unwrap_or(None);
        if current_holder == Some(self.client_id.clone()) {
            let _: () = redis::cmd("DEL").arg(&self.lock_key).query_async(&mut con).await.unwrap_or(());
        }''',
    content
)

# Replace RELEASE_SCRIPT block 2
content = re.sub(
    r'let _: Result<i32> = RELEASE_SCRIPT\s*\.key\(&lock_key\)\s*\.arg\(&client_id\)\s*\.invoke_async\(&mut con\)\s*\.await[^;]*;',
    r'''let current_holder: Option<String> = redis::cmd("GET").arg(&lock_key).query_async(&mut con).await.unwrap_or(None);
                    if current_holder == Some(client_id.clone()) {
                        let _: () = redis::cmd("DEL").arg(&lock_key).query_async(&mut con).await.unwrap_or(());
                    }''',
    content
)

# Replace DELEGATION_RELEASE_SCRIPT block 1
content = re.sub(
    r'let _: i32 = DELEGATION_RELEASE_SCRIPT\s*\.key\(&self\.delegation_key\)\s*\.arg\(&self\.client_id\)\s*\.invoke_async\(&mut con\)\s*\.await[^;]*;',
    r'''let current_holder: Option<String> = redis::cmd("GET").arg(&self.delegation_key).query_async(&mut con).await.unwrap_or(None);
        if current_holder == Some(self.client_id.clone()) {
            let _: () = redis::cmd("DEL").arg(&self.delegation_key).query_async(&mut con).await.unwrap_or(());
        }''',
    content
)

# Replace DELEGATION_RELEASE_SCRIPT block 2
content = re.sub(
    r'let _: Result<i32> = DELEGATION_RELEASE_SCRIPT\s*\.key\(&key\)\s*\.arg\(&client_id\)\s*\.invoke_async\(&mut con\)\s*\.await[^;]*;',
    r'''let current_holder: Option<String> = redis::cmd("GET").arg(&key).query_async(&mut con).await.unwrap_or(None);
                    if current_holder == Some(client_id.clone()) {
                        let _: () = redis::cmd("DEL").arg(&key).query_async(&mut con).await.unwrap_or(());
                    }''',
    content
)

open('src/dlm.rs', 'w').write(content)
