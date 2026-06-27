import sys
content = open('src/dlm.rs').read()
if 'println!("REDIS ERROR' not in content:
    content = content.replace('.await?;', '.await.map_err(|e| { println!("REDIS ERROR: {:?}", e); e })?;')
    with open('src/dlm.rs', 'w') as f:
        f.write(content)
