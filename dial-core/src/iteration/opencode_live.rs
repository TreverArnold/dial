//! Linux OpenCode viewer for the exact worker session. Opt in with opencode_live=true.
use crate::errors::{DialError, Result};
use serde_json::Value;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(super) struct LiveSession {
    server: Child,
    pub url: String,
    pub id: String,
    directory: String,
}

impl LiveSession {
    pub fn permission_rejection(&self) -> Result<Option<String>> {
        let messages = request(
            &format!(
                "{}/session/{}/message?directory={}&limit=20",
                self.url, self.id, self.directory
            ),
            None,
        )?;
        Ok(super::opencode_permission::rejected_message(&messages))
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

fn request(url: &str, body: Option<&str>) -> Result<Value> {
    let mut cmd = Command::new("curl");
    // Creating a session can initialize the project after the health endpoint is
    // ready. Allow that cold start without slowing health/permission polling.
    let timeout = if body.is_some() { "30" } else { "3" };
    cmd.args(["--fail", "--silent", "--show-error", "--max-time", timeout]);
    if let Some(body) = body {
        cmd.args([
            "--request",
            "POST",
            "--header",
            "Content-Type: application/json",
            "--data",
            body,
        ]);
    }
    let result = cmd
        .arg(url)
        .output()
        .map_err(|e| DialError::CommandFailed(format!("OpenCode HTTP client unavailable: {e}")))?;
    if !result.status.success() {
        return Err(DialError::CommandFailed(format!(
            "OpenCode HTTP request failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    serde_json::from_slice(&result.stdout)
        .map_err(|e| DialError::CommandFailed(format!("Invalid OpenCode response: {e}")))
}

pub(super) fn start(port: u16, title: &str) -> Result<LiveSession> {
    if !cfg!(target_os = "linux") {
        return Err(DialError::InvalidConfig(
            "opencode_live currently requires Linux and Konsole".into(),
        ));
    }
    if Command::new("konsole")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        return Err(DialError::CommandFailed(
            "opencode_live requires Konsole".into(),
        ));
    }
    let url = format!("http://127.0.0.1:{port}");
    if request(&format!("{url}/global/health"), None).is_ok() {
        return Err(DialError::CommandFailed(format!(
            "Port {port} already has an OpenCode server; choose another opencode_live_port"
        )));
    }
    let mut server = Command::new("opencode")
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| DialError::CommandFailed(format!("Could not start OpenCode server: {e}")))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if request(&format!("{url}/global/health"), None).is_ok() {
            break;
        }
        if server.try_wait().ok().flatten().is_some() {
            let _ = server.wait();
            return Err(DialError::CommandFailed(
                "OpenCode server exited during startup".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if request(&format!("{url}/global/health"), None).is_err() {
        let _ = server.kill();
        let _ = server.wait();
        return Err(DialError::CommandFailed(
            "OpenCode server did not become healthy".into(),
        ));
    }
    let mut live = LiveSession {
        server,
        url,
        id: String::new(),
        directory: String::new(),
    };
    let directory = std::env::current_dir()?;
    let encoded: String = directory
        .to_string_lossy()
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    live.directory = encoded.clone();
    let endpoint = format!("{}/session?directory={encoded}", live.url);
    let body = serde_json::json!({"title": title}).to_string();
    let result = request(&endpoint, Some(&body))?;
    let id = result
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| DialError::CommandFailed("OpenCode session response has no ID".into()))?
        .to_string();
    live.id = id.clone();
    let viewer = Command::new("konsole")
        .args([
            "--hold",
            "--workdir",
            &directory.to_string_lossy(),
            "-e",
            "bash",
            "-lc",
            "exec opencode attach \"$1\" --session \"$2\"",
            "dial-viewer",
            &live.url,
            &id,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| DialError::CommandFailed(format!("Could not open OpenCode viewer: {e}")))?;
    println!(
        "OpenCode session {id} is opening in Konsole. Backend: {}",
        live.url
    );
    drop(viewer);
    Ok(live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn server(delay: Duration, status: &str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/session", listener.local_addr().unwrap());
        let status = status.to_owned();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            std::thread::sleep(delay);
            let body = r#"{"id":"ses_delayed"}"#;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (url, handle)
    }

    #[test]
    fn session_creation_allows_cold_start_longer_than_health_deadline() {
        let (url, handle) = server(Duration::from_millis(3300), "200 OK");
        let result = request(&url, Some("{}"));
        handle.join().unwrap();
        assert_eq!(result.unwrap()["id"], "ses_delayed");
    }

    #[test]
    fn health_request_keeps_short_deadline() {
        let (url, handle) = server(Duration::from_millis(3300), "200 OK");
        let result = request(&url, None);
        handle.join().unwrap();
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn session_creation_still_rejects_http_failure() {
        let (url, handle) = server(Duration::ZERO, "503 Service Unavailable");
        let result = request(&url, Some("{}"));
        handle.join().unwrap();
        assert!(result.is_err());
    }
}
