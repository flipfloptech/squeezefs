import sys

content = open('src/dlm.rs').read()

old = """                match pipe_get.query_async::<Vec<Option<String>>>(&mut con).await {"""
new = """                let holders_res: redis::RedisResult<Vec<Option<String>>> = pipe_get.query_async(&mut con).await;
                match holders_res {"""

if old in content:
    content = content.replace(old, new)
    open('src/dlm.rs', 'w').write(content)
    print("Replaced pipe_get.query_async")
else:
    print("Failed to replace pipe_get.query_async")
