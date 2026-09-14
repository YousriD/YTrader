//! Local rendezvous point for yTrader and the MT5-native Expert Advisor.
//! It binds only to loopback. The EA publishes account state and polls for
//! commands; the existing Rust broker keeps the same HTTP protocol as before.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Bridge {
    state: Value,
    queued: VecDeque<Value>,
    results: HashMap<String, Value>,
    next_id: u64,
}

fn value_at<'a>(state: &'a Value, key: &str, fallback: Value) -> Value {
    state.get(key).cloned().unwrap_or(fallback)
}

fn respond(stream: &mut TcpStream, status: u16, body: Value) {
    let text = body.to_string();
    let phrase = match status { 200 => "OK", 204 => "No Content", 400 => "Bad Request", 404 => "Not Found", 503 => "Service Unavailable", _ => "Error" };
    let _ = write!(stream, "HTTP/1.1 {status} {phrase}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", text.len(), text);
}

fn request(stream: &mut TcpStream) -> Option<(String, String, Value)> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 { break; }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") { break; }
        if bytes.len() > 64 * 1024 { return None; }
    }
    let header_end = bytes.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let first = headers.lines().next()?.split_whitespace().collect::<Vec<_>>();
    if first.len() < 2 { return None; }
    let length = headers.lines().find_map(|line| line.strip_prefix("Content-Length:").or_else(|| line.strip_prefix("content-length:"))).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
    while bytes.len() < header_end + length {
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 { return None; }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let body = if length == 0 { json!({}) } else { serde_json::from_slice(&bytes[header_end..header_end + length]).ok()? };
    Some((first[0].to_string(), first[1].to_string(), body))
}

fn query(path: &str, name: &str) -> Option<String> {
    path.split_once('?').and_then(|(_, q)| q.split('&').find_map(|p| p.split_once('=').filter(|(k, _)| *k == name).map(|(_, v)| v.to_string())))
}

fn handle(mut stream: TcpStream, bridge: Arc<Mutex<Bridge>>) {
    let Some((method, path, body)) = request(&mut stream) else { return; };
    let route = path.split('?').next().unwrap_or(&path);
    if method == "POST" && route == "/ea/state" {
        bridge.lock().unwrap().state = body;
        respond(&mut stream, 200, json!({"ok": true}));
        return;
    }
    if method == "GET" && route == "/ea/next_order" {
        let command = bridge.lock().unwrap().queued.pop_front();
        match command { Some(v) => respond(&mut stream, 200, v), None => respond(&mut stream, 204, json!({})) }
        return;
    }
    if method == "POST" && route == "/ea/order_result" {
        let Some(id) = body.get("id").and_then(Value::as_str) else { respond(&mut stream, 400, json!({"error":"missing id"})); return; };
        bridge.lock().unwrap().results.insert(id.to_string(), body);
        respond(&mut stream, 200, json!({"ok": true}));
        return;
    }

    if method == "POST" && route == "/order" {
        let mut command = body;
        let id = {
            let mut b = bridge.lock().unwrap();
            b.next_id += 1;
            b.next_id.to_string()
        };
        command["id"] = Value::String(id.clone());
        bridge.lock().unwrap().queued.push_back(command);
        let deadline = Instant::now() + Duration::from_secs(12);
        while Instant::now() < deadline {
            if let Some(result) = bridge.lock().unwrap().results.remove(&id) { respond(&mut stream, 200, result); return; }
            thread::sleep(Duration::from_millis(50));
        }
        respond(&mut stream, 503, json!({"error":"MT5 EA did not acknowledge order within 12 seconds"}));
        return;
    }

    let state = bridge.lock().unwrap().state.clone();
    let response = match (method.as_str(), route) {
        ("GET", "/health") => value_at(&state, "health", json!({"connected":false,"trade_mode":"UNKNOWN"})),
        ("GET", "/account") => value_at(&state, "account", json!({"error":"EA has not published account state"})),
        ("GET", "/symbol_info") => query(&path, "symbol").and_then(|s| state.get("symbols")?.get(s).cloned()).unwrap_or(json!({"error":"unknown symbol"})),
        ("GET", "/positions") => query(&path, "symbol").and_then(|s| state.get("positions")?.get(s).cloned()).map(|p| json!({"positions":p})).unwrap_or(json!({"positions":[]})),
        ("GET", "/price") => query(&path, "symbol").and_then(|s| state.get("prices")?.get(s).cloned()).unwrap_or(json!({"error":"no price"})),
        _ => { respond(&mut stream, 404, json!({"error":"unknown route"})); return; }
    };
    respond(&mut stream, 200, response);
}

fn main() {
    let listener = TcpListener::bind("127.0.0.1:5001").expect("cannot bind 127.0.0.1:5001");
    println!("yTrader MT5 native sidecar listening on http://127.0.0.1:5001");
    let bridge = Arc::new(Mutex::new(Bridge::default()));
    for stream in listener.incoming().flatten() {
        let bridge = bridge.clone();
        thread::spawn(move || handle(stream, bridge));
    }
}
