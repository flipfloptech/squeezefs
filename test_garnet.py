import redis
try:
    for i in range(1, 11):
        r = redis.Redis(host='127.0.0.1', port=6379, db=i)
        r.set('foo', f'bar{i}')
    
    for i in range(1, 11):
        r = redis.Redis(host='127.0.0.1', port=6379, db=i)
        val = r.get('foo').decode('utf-8')
        print(f"DB {i}: {val}")
    
    # test flushdb
    r = redis.Redis(host='127.0.0.1', port=6379, db=1)
    r.flushdb()
    val = r.get('foo')
    print(f"DB 1 after flush: {val}")
    
    r2 = redis.Redis(host='127.0.0.1', port=6379, db=2)
    val2 = r2.get('foo').decode('utf-8')
    print(f"DB 2 after DB 1 flush: {val2}")
    
except Exception as e:
    print(f"Error: {e}")
