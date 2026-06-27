#[tokio::main]
async fn main() {
    let client1 = redis::Client::open("redis://127.0.0.1:6379/1").unwrap();
    let mut con1 = client1.get_multiplexed_tokio_connection().await.unwrap();
    let client2 = redis::Client::open("redis://127.0.0.1:6379/2").unwrap();
    let mut con2 = client2.get_multiplexed_tokio_connection().await.unwrap();
    
    let _: () = redis::cmd("SET").arg("foo").arg("bar1").query_async(&mut con1).await.unwrap();
    let _: () = redis::cmd("SET").arg("foo").arg("bar2").query_async(&mut con2).await.unwrap();
    
    let _: () = redis::cmd("FLUSHDB").query_async(&mut con1).await.unwrap();
    
    let v2: Option<String> = redis::cmd("GET").arg("foo").query_async(&mut con2).await.unwrap();
    println!("DB 2 after DB 1 flush: {:?}", v2);
}
