//! Synthetic local OpenCode double; never contacts an external service.
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};
fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = args.get(1).map_or("serve", String::as_str);
    if mode == "exit" {
        std::process::exit(2);
    }
    let root = std::env::current_dir().unwrap();
    let runtime = root.parent().unwrap().join("runtime");
    std::fs::write(runtime.join("fixture.pid"), std::process::id().to_string()).unwrap();
    if mode == "stall" {
        std::thread::sleep(Duration::from_secs(30));
        return;
    }
    let port = args
        .windows(2)
        .find_map(|pair| (pair[0] == "--port").then(|| pair[1].parse::<u16>().unwrap()))
        .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    println!("fixture ready");
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut raw = Vec::new();
        let mut buf = [0; 1024];
        while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }
        let request = String::from_utf8_lossy(&raw);
        let path = request
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("");
        let mut doc: serde_json::Value =
            serde_json::from_str(include_str!("openapi.json")).unwrap();
        if mode == "no-model" {
            doc["paths"]["/session/{sessionID}/prompt_async"]["post"]["requestBody"]["content"]["application/json"]["schema"]["properties"].as_object_mut().unwrap().remove("model");
        }
        let body=match path {
   "/global/health"=>serde_json::json!({"healthy":mode!="unhealthy","version":"fixture"}),
   "/path"=>serde_json::json!({"directory":if mode=="bad-path"{root.parent().unwrap().to_str().unwrap()}else{root.to_str().unwrap()}}),
   "/doc"=>if mode=="bad-doc"{serde_json::json!({})}else{doc},
   "/session"=>serde_json::json!([]),
   _=>serde_json::json!({}),
  }.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
    }
}
