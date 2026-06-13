use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug)]
enum Outbound {
    Request {
        id: u64,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct JsonRpcResponse {
    pub id: u64,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

pub struct JsonRpcTransport {
    outbound_tx: Sender<Outbound>,
    response_rx: Receiver<JsonRpcResponse>,
    notification_rx: Receiver<Value>,
    next_id: AtomicU64,
    child: Mutex<Option<Child>>,
    reader_failed: Arc<AtomicBool>,
    default_timeout: Duration,
}

impl JsonRpcTransport {
    pub fn spawn(program: &str, args: &[String], cwd: &std::path::Path) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn LSP server {program}"))?;

        let stdin = child.stdin.take().context("LSP server missing stdin")?;
        let stdout = child.stdout.take().context("LSP server missing stdout")?;

        let (outbound_tx, outbound_rx) = mpsc::channel::<Outbound>();
        let (response_tx, response_rx) = mpsc::channel::<JsonRpcResponse>();
        let (notification_tx, notification_rx) = mpsc::channel::<Value>();
        let reader_failed = Arc::new(AtomicBool::new(false));
        let reader_failed_flag = Arc::clone(&reader_failed);

        thread::spawn(move || writer_loop(stdin, outbound_rx));
        thread::spawn(move || {
            reader_loop(stdout, response_tx, notification_tx, reader_failed_flag)
        });

        Ok(Self {
            outbound_tx,
            response_rx,
            notification_rx,
            next_id: AtomicU64::new(1),
            child: Mutex::new(Some(child)),
            reader_failed,
            default_timeout: Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS),
        })
    }

    pub fn set_request_timeout(&mut self, timeout: Duration) {
        self.default_timeout = timeout;
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, self.default_timeout)
    }

    pub fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        if self.reader_failed.load(Ordering::SeqCst) {
            return Err(anyhow!("LSP reader thread exited unexpectedly"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.outbound_tx
            .send(Outbound::Request {
                id,
                method: method.to_string(),
                params,
            })
            .map_err(|_| anyhow!("LSP writer channel closed"))?;

        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("LSP request {method} timed out after {timeout:?}"));
            }
            match self.response_rx.recv_timeout(remaining) {
                Ok(response) if response.id == id => {
                    if let Some(error) = response.error {
                        return Err(anyhow!("LSP error for {method}: {error}"));
                    }
                    return Ok(response.result.unwrap_or(Value::Null));
                }
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(anyhow!("LSP request {method} timed out after {timeout:?}"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("LSP response channel closed"));
                }
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.outbound_tx
            .send(Outbound::Notification {
                method: method.to_string(),
                params,
            })
            .map_err(|_| anyhow!("LSP writer channel closed"))
    }

    pub fn drain_notifications(&self) -> Vec<Value> {
        let mut items = Vec::new();
        while let Ok(notification) = self.notification_rx.try_recv() {
            items.push(notification);
        }
        items
    }

    pub fn wait_notification(&self, method: &str, timeout: Duration) -> Result<Option<Value>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match self.notification_rx.recv_timeout(remaining) {
                Ok(notification) => {
                    if notification.get("method").and_then(Value::as_str) == Some(method) {
                        return Ok(Some(notification));
                    }
                    if notification.get("method").and_then(Value::as_str) == Some("$/progress") {
                        if let Some(params) = notification.get("params") {
                            if params
                                .get("value")
                                .and_then(|v| v.get("kind"))
                                .and_then(Value::as_str)
                                == Some("end")
                            {
                                return Ok(Some(notification));
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => return Ok(None),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("LSP notification channel closed"));
                }
            }
        }
    }

    pub fn shutdown_transport(&self) -> Result<()> {
        let _ = self.outbound_tx.send(Outbound::Shutdown);
        Ok(())
    }

    pub fn kill(&self) -> Result<()> {
        let mut guard = self
            .child
            .lock()
            .map_err(|_| anyhow!("LSP child lock poisoned"))?;
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }
}

impl Drop for JsonRpcTransport {
    fn drop(&mut self) {
        let _ = self.shutdown_transport();
        let _ = self.kill();
    }
}

fn writer_loop(mut stdin: ChildStdin, outbound_rx: Receiver<Outbound>) {
    while let Ok(message) = outbound_rx.recv() {
        match message {
            Outbound::Request { id, method, params } => {
                let payload = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                });
                if write_message(&mut stdin, &payload).is_err() {
                    break;
                }
            }
            Outbound::Notification { method, params } => {
                let payload = json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params,
                });
                if write_message(&mut stdin, &payload).is_err() {
                    break;
                }
            }
            Outbound::Shutdown => break,
        }
    }
}

fn reader_loop(
    stdout: ChildStdout,
    response_tx: Sender<JsonRpcResponse>,
    notification_tx: Sender<Value>,
    reader_failed: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_message(&mut reader) {
            Ok(Some(message)) => {
                if message.get("method").is_some() && message.get("id").is_none() {
                    let _ = notification_tx.send(message);
                    continue;
                }
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let _ = response_tx.send(JsonRpcResponse {
                        id,
                        result: message.get("result").cloned(),
                        error: message.get("error").cloned(),
                    });
                }
            }
            Ok(None) => break,
            Err(_) => {
                reader_failed.store(true, Ordering::SeqCst);
                break;
            }
        }
    }
}

fn write_message(writer: &mut impl Write, payload: &Value) -> Result<()> {
    let body = serde_json::to_string(payload)?;
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    writer.flush()?;
    Ok(())
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut header = String::new();
        let bytes = reader.read_line(&mut header)?;
        if bytes == 0 {
            return Ok(None);
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some(rest) = header.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse::<usize>()?);
        }
    }
    let Some(length) = content_length else {
        return Err(anyhow!("LSP frame missing Content-Length"));
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let message = serde_json::from_slice(&body)?;
    Ok(Some(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_read_roundtrip() {
        let payload = json!({"jsonrpc":"2.0","id":1,"result":{}});
        let mut buffer = Vec::new();
        write_message(&mut buffer, &payload).unwrap();
        let mut cursor = std::io::Cursor::new(buffer);
        let read = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(read["id"], 1);
    }
}
