import os

test_dir = r"c:\Users\JustinOberdorf\Source\squeezefs\tests"

def strip_rust_comments(text):
    lines = []
    for line in text.split('\n'):
        # Strip comments starting with // (none of the string literals in our test calls have '//')
        if "//" in line:
            line = line.split("//")[0]
        lines.append(line)
    return "\n".join(lines)

def replace_tiered_cache_new(content):
    idx = 0
    result = []
    while idx < len(content):
        pos = content.find("TieredCache::new(", idx)
        if pos == -1:
            result.append(content[idx:])
            break
        result.append(content[idx:pos])
        start_args = pos + len("TieredCache::new(")
        depth = 1
        args_end = start_args
        while args_end < len(content) and depth > 0:
            char = content[args_end]
            if char == '(':
                depth += 1
            elif char == ')':
                depth -= 1
            args_end += 1
        
        if depth == 0:
            args_str = content[start_args:args_end-1]
            clean_args_str = strip_rust_comments(args_str)
            args = []
            current = []
            bracket_depth = 0
            for char in clean_args_str:
                if char in ('(', '[', '{'):
                    bracket_depth += 1
                elif char in (')', ']', '}'):
                    bracket_depth -= 1
                if char == ',' and bracket_depth == 0:
                    args.append("".join(current).strip())
                    current = []
                else:
                    current.append(char)
            if current:
                args.append("".join(current).strip())
            
            if args and args[-1] == "":
                args.pop()
                
            if len(args) == 5:
                mem1 = args[1]
                mem2 = args[1]
                disk1 = args[2]
                disk2 = args[2]
                new_args = [args[0], mem1, mem2, disk1, disk2, args[3], args[4]]
                replaced = "TieredCache::new(\n        " + ",\n        ".join(new_args) + "\n    )"
                result.append(replaced)
            else:
                result.append(content[pos:args_end])
            idx = args_end
        else:
            result.append("TieredCache::new(")
            idx = start_args
    return "".join(result)

def replace_format_volume(content):
    idx = 0
    result = []
    while idx < len(content):
        pos = content.find("format_volume(", idx)
        if pos == -1:
            result.append(content[idx:])
            break
        result.append(content[idx:pos])
        start_args = pos + len("format_volume(")
        depth = 1
        args_end = start_args
        while args_end < len(content) and depth > 0:
            char = content[args_end]
            if char == '(':
                depth += 1
            elif char == ')':
                depth -= 1
            args_end += 1
        
        if depth == 0:
            args_str = content[start_args:args_end-1]
            clean_args_str = strip_rust_comments(args_str)
            args = []
            current = []
            bracket_depth = 0
            for char in clean_args_str:
                if char in ('(', '[', '{'):
                    bracket_depth += 1
                elif char in (')', ']', '}'):
                    bracket_depth -= 1
                if char == ',' and bracket_depth == 0:
                    args.append("".join(current).strip())
                    current = []
                else:
                    current.append(char)
            if current:
                args.append("".join(current).strip())
            
            if args and args[-1] == "":
                args.pop()
                
            if len(args) == 15:
                new_args = args + ["None", "None", "None", "None"]
                replaced = "format_volume(\n        " + ",\n        ".join(new_args) + "\n    )"
                result.append(replaced)
            else:
                result.append(content[pos:args_end])
            idx = args_end
        else:
            result.append("format_volume(")
            idx = start_args
    return "".join(result)

def replace_lru_remove(content):
    idx = 0
    result = []
    while idx < len(content):
        pos = content.find(".lru.remove(", idx)
        if pos == -1:
            result.append(content[idx:])
            break
        start_args = pos + len(".lru.remove(")
        depth = 1
        args_end = start_args
        while args_end < len(content) and depth > 0:
            char = content[args_end]
            if char == '(':
                depth += 1
            elif char == ')':
                depth -= 1
            args_end += 1
        
        if depth == 0:
            back_pos = pos - 1
            while back_pos >= 0:
                char = content[back_pos]
                if char in ('\n', ';', '{', '}'):
                    back_pos += 1
                    break
                back_pos -= 1
            if back_pos < 0:
                back_pos = 0
            
            statement_prefix = content[back_pos:pos].strip()
            args_str = content[start_args:args_end-1]
            replaced = f"{statement_prefix}.write_lru.remove({args_str}); {statement_prefix}.read_lru.remove({args_str})"
            result.append(content[idx:back_pos])
            result.append(replaced)
            idx = args_end
        else:
            result.append(".lru.remove(")
            idx = start_args
    return "".join(result)

for filename in os.listdir(test_dir):
    if filename.endswith(".rs"):
        path = os.path.join(test_dir, filename)
        with open(path, "r", encoding="utf-8") as f:
            content = f.read()
        
        new_content = replace_tiered_cache_new(content)
        new_content = replace_format_volume(new_content)
        new_content = replace_lru_remove(new_content)
        
        if new_content != content:
            with open(path, "w", encoding="utf-8") as f:
                f.write(new_content)
            print(f"Updated {filename}")
