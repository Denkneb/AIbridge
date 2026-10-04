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
        let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let length = String::from_utf8_lossy(&raw[..header_end])
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        while raw.len() < header_end + length {
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }
        let data: serde_json::Value =
            serde_json::from_slice(&raw[header_end..]).unwrap_or_default();
        let request = String::from_utf8_lossy(&raw);
        let method = request.split_whitespace().next().unwrap_or("");
        let path = request
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("");
        let mode = std::fs::read_to_string(runtime.join("fixture-mode"))
            .unwrap_or_else(|_| mode.to_owned());
        let mut doc: serde_json::Value =
            serde_json::from_str(include_str!("openapi.json")).unwrap();
        if mode == "no-model" {
            doc["paths"]["/session/{sessionID}/prompt_async"]["post"]["requestBody"]["content"]["application/json"]["schema"]["properties"].as_object_mut().unwrap().remove("model");
        }
        {
            use std::fs::OpenOptions;
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(runtime.join("fixture-requests.log"))
                .unwrap();
            writeln!(log, "{method} {path}").unwrap();
        }
        if path == "/path" && runtime.join("pause-path").exists() {
            std::fs::write(runtime.join("path-seen"), "seen").unwrap();
            let limit = std::time::Instant::now() + Duration::from_secs(2);
            while !runtime.join("resume-path").exists() && std::time::Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let body=match path {
   "/global/health"=>serde_json::json!({"healthy":mode!="unhealthy","version":"fixture"}),
   "/path"=>serde_json::json!({"directory":if mode=="bad-path"{root.parent().unwrap().to_str().unwrap()}else{root.to_str().unwrap()}}),
   "/doc"=>if mode=="bad-doc"{serde_json::json!({})}else{doc},
   "/session" if method=="POST"=>serde_json::json!({"id":format!("ses_fixture_{}",data["title"].as_str().unwrap_or("").split_whitespace().last().unwrap_or("1")),"title":data["title"],"directory":root}),
   "/session"=>serde_json::json!([]),
   "/permission"=>if mode=="permission"{serde_json::json!([{"id":"per_fixture","sessionID":"ses_fixture_1","permission":"bash","patterns":["pwd"]}])}else{serde_json::json!([])},
   "/question"=>if mode=="question"{serde_json::json!([{"id":"que_fixture","sessionID":"ses_fixture_1","questions":[]}])}else if mode=="foreign-question"{serde_json::json!([{"id":"que_foreign","sessionID":"ses_other","questions":[]}])}else{serde_json::json!([])},
   _=>serde_json::json!({}),
  }.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        if path.ends_with("/prompt_async") {
            use std::fs::OpenOptions;
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(runtime.join("fixture-prompts.jsonl"))
                .unwrap();
            writeln!(file, "{data}").unwrap();
        }
        let _ = stream.write_all(response.as_bytes());
    }
}
