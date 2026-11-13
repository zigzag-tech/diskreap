//! Client of the Benchday daemon's local stream ingress
//! (`~/.local/state/benchday-daemon/streams-ingress.sock`, same uid only).
//!
//! Wire: 4-byte big-endian length + JSON, at most 64 KiB per frame. Every frame we
//! send is answered by exactly one reply, in order, except `write_result`; the
//! daemon also sends unsolicited `lease`, `mode`, `write`, `authority_lost` and
//! `authority_epoch` frames. The reference client is Benchday's
//! `packages/benchday-plugin-api/sdk/python/benchday_streams/client.py`.
//!
//! The daemon allows 50 writes/s per connection; we hold ourselves to 40 so a
//! scan publishing hundreds of candidates is throttled here instead of being
//! answered `rate_limited` there.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_FRAME: usize = 65536;
const WRITES_PER_S: f64 = 40.0;
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

pub fn socket_path() -> Option<PathBuf> {
    if let Some(p) = crate::util::config_json()
        .and_then(|c| c.get("ingress_socket").and_then(Value::as_str).map(crate::util::expand_tilde))
    {
        return Some(p);
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("benchday-daemon/streams-ingress.sock"))
}

/// Something the daemon told us without being asked.
pub enum Event {
    Lease { contract: String, live: bool },
    Write(Value),
    /// The session ended (socket closed, malformed frame, authority lost). Reconnect.
    Down(String),
}

struct Writer {
    stream: UnixStream,
    tokens: f64,
    last: Instant,
}

#[derive(Clone)]
pub struct Conn {
    writer: Arc<Mutex<Writer>>,
    waiting: Arc<Mutex<VecDeque<Sender<Value>>>>,
    pub daemon: String,
}

pub fn encode(value: &Value) -> Result<Vec<u8>, String> {
    let body = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if body.len() > MAX_FRAME {
        return Err(format!("frame of {} bytes exceeds 64 KiB", body.len()));
    }
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    Ok(out)
}

fn read_frame(r: &mut impl Read) -> Result<Value, String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).map_err(|e| format!("socket closed: {e}"))?;
    let n = u32::from_be_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(format!("daemon frame of {n} bytes exceeds 64 KiB"));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).map_err(|e| format!("socket closed: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("malformed frame: {e}"))
}

impl Conn {
    /// Connect, say hello, and start the reader. Events go to the returned receiver.
    pub fn open(path: &std::path::Path) -> Result<(Conn, Receiver<Event>), String> {
        let stream = UnixStream::connect(path).map_err(|e| format!("ingress {}: {e}", path.display()))?;
        let mut reader = stream.try_clone().map_err(|e| e.to_string())?;
        let waiting: Arc<Mutex<VecDeque<Sender<Value>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (events, rx) = channel();
        let w = waiting.clone();
        std::thread::spawn(move || {
            let why = loop {
                let frame = match read_frame(&mut reader) {
                    Ok(f) => f,
                    Err(e) => break e,
                };
                let kind = frame.get("type").and_then(Value::as_str).unwrap_or("").to_string();
                match kind.as_str() {
                    "lease" => {
                        let contract = frame["contract"].as_str().unwrap_or("").to_string();
                        let live = frame["live"].as_bool().unwrap_or(false);
                        if events.send(Event::Lease { contract, live }).is_err() {
                            break "event receiver gone".into();
                        }
                    }
                    "write" => {
                        if events.send(Event::Write(frame)).is_err() {
                            break "event receiver gone".into();
                        }
                    }
                    // The daemon retired a registration: start over so the refusal stays visible.
                    "authority_lost" => break format!("authority lost for {}", frame["contract"]),
                    "mode" | "authority_epoch" => {}
                    _ => match w.lock().unwrap().pop_front() {
                        Some(waiter) => {
                            let _ = waiter.send(frame);
                        }
                        None => break format!("reply without a request: {kind}"),
                    },
                }
            };
            // Dropping the waiters fails every pending request; none is left hanging.
            w.lock().unwrap().clear();
            let _ = events.send(Event::Down(why));
        });
        let conn = Conn {
            writer: Arc::new(Mutex::new(Writer { stream, tokens: WRITES_PER_S, last: Instant::now() })),
            waiting,
            daemon: String::new(),
        };
        let hello = conn.request(json!({"type": "hello", "package": "benchday.disk-cleanup", "version": "1"}))?;
        let daemon = hello["daemon"].as_str().unwrap_or("").to_string();
        if daemon.is_empty() {
            return Err("hello_ok named no daemon".into());
        }
        Ok((Conn { daemon, ..conn }, rx))
    }

    fn send(&self, frame: &Value, waiter: Option<Sender<Value>>) -> Result<(), String> {
        let bytes = encode(frame)?;
        let mut w = self.writer.lock().unwrap();
        let is_write = matches!(
            frame["type"].as_str(),
            Some("publish" | "intent_partial" | "serve" | "submit" | "step" | "cancel")
        );
        if is_write {
            let now = Instant::now();
            w.tokens = (w.tokens + now.duration_since(w.last).as_secs_f64() * WRITES_PER_S).min(WRITES_PER_S);
            w.last = now;
            if w.tokens < 1.0 {
                std::thread::sleep(Duration::from_secs_f64((1.0 - w.tokens) / WRITES_PER_S));
                w.tokens = 1.0;
                w.last = Instant::now();
            }
            w.tokens -= 1.0;
        }
        // Register the waiter under the writer lock: replies come back in send order.
        if let Some(waiter) = waiter {
            self.waiting.lock().unwrap().push_back(waiter);
        }
        w.stream.write_all(&bytes).map_err(|e| format!("ingress write: {e}"))
    }

    /// Send a frame and wait for its reply. An `error` reply is `Err("code: detail")`.
    pub fn request(&self, frame: Value) -> Result<Value, String> {
        let (tx, rx) = channel();
        self.send(&frame, Some(tx))?;
        let reply = rx.recv_timeout(REPLY_TIMEOUT).map_err(|_| "ingress reply timed out or session ended".to_string())?;
        if reply["type"] == "error" {
            return Err(format!(
                "{}: {}",
                reply["code"].as_str().unwrap_or("error"),
                reply["detail"].as_str().unwrap_or("")
            ));
        }
        Ok(reply)
    }

    /// Answer a forwarded write. No reply follows.
    pub fn answer(&self, req: &str, partial: &Value) -> Result<(), String> {
        self.send(&json!({"type": "write_result", "req": req, "partial": partial}), None)
    }

    /// Serve a contract (intent authority or native state producer); returns the epoch.
    pub fn serve(&self, contract: &str) -> Result<u64, String> {
        let reply = self.request(json!({"type": "serve", "contract": contract}))?;
        if reply["type"] != "serving" {
            return Err(format!("serve {contract}: unexpected {}", reply["type"]));
        }
        Ok(reply["epoch"].as_u64().unwrap_or(0))
    }

    pub fn publish(&self, contract: &str, subject: Value, body: Value) -> Result<Value, String> {
        self.request(json!({"type": "publish", "contract": contract, "subject": subject, "status": "ok", "body": body}))
    }

    pub fn intent_partial(&self, contract: &str, partial: &Value) -> Result<Value, String> {
        self.request(json!({"type": "intent_partial", "contract": contract, "partial": partial}))
    }
}

/// The partial an intent authority mints. The service owns epoch (from `serve`) and seq.
pub fn intent_partial(contract: &str, daemon: &str, epoch: u64, seq: u64, body: &Value) -> Value {
    json!({
        "contract": contract,
        "source": format!("daemon:{daemon}"),
        "subject": {"intent_id": body["intent_id"]},
        "epoch": epoch,
        "seq": seq,
        "produced_ms": crate::util::now() * 1000,
        "ttl_ms": 0,
        "status": "ok",
        "body": body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_length_prefixed_and_bounded() {
        let f = encode(&json!({"type": "hello"})).unwrap();
        assert_eq!(&f[..4], &(16u32).to_be_bytes());
        assert_eq!(read_frame(&mut &f[..]).unwrap()["type"], "hello");
        let big = json!({"x": "a".repeat(MAX_FRAME)});
        assert!(encode(&big).is_err());
    }

    #[test]
    fn replies_match_requests_in_order_and_events_are_separate() {
        let dir = std::env::temp_dir().join(format!("diskreap-ingress-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("s.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let hello = read_frame(&mut s).unwrap();
            assert_eq!(hello["type"], "hello");
            s.write_all(&encode(&json!({"type": "hello_ok", "daemon": "box"})).unwrap()).unwrap();
            let serve = read_frame(&mut s).unwrap();
            // An unsolicited lease arrives before the reply; it must not be taken as the answer.
            s.write_all(&encode(&json!({"type": "lease", "contract": "c/1", "live": true})).unwrap()).unwrap();
            s.write_all(&encode(&json!({"type": "serving", "contract": serve["contract"], "epoch": 7})).unwrap()).unwrap();
        });
        let (conn, events) = Conn::open(&sock).unwrap();
        assert_eq!(conn.daemon, "box");
        assert_eq!(conn.serve("c/1").unwrap(), 7);
        match events.recv_timeout(Duration::from_secs(2)).unwrap() {
            Event::Lease { contract, live } => assert!(contract == "c/1" && live),
            _ => panic!("expected a lease event"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
