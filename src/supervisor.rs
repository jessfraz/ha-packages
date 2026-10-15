//! Announce only the dedicated adapter credential through Supervisor's
//! authenticated discovery channel. No credentials enter the web dashboard.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone)]
pub struct Supervisor {
    client: Client,
    token: String,
    origin: String,
}

#[derive(Debug, thiserror::Error)]
#[error("Home Assistant discovery is unavailable; retrying")]
pub struct DiscoveryError;

#[derive(Deserialize)]
struct Envelope<T> {
    result: String,
    data: T,
}

#[derive(Deserialize)]
struct AppInfo {
    hostname: String,
}

#[derive(Serialize)]
struct Announcement<'a> {
    service: &'static str,
    config: Connection<'a>,
}

#[derive(Serialize)]
struct Connection<'a> {
    backend_url: String,
    token: &'a str,
}

impl Supervisor {
    pub fn from_environment() -> Result<Option<Self>, DiscoveryError> {
        let Some(token) = std::env::var("SUPERVISOR_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
        else {
            return Ok(None);
        };
        Self::new(token, "http://supervisor".into()).map(Some)
    }

    fn new(token: String, origin: String) -> Result<Self, DiscoveryError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| DiscoveryError)?;
        Ok(Self {
            client,
            token,
            origin,
        })
    }

    pub async fn announce(&self, integration_token: &str) -> Result<(), DiscoveryError> {
        let response = self
            .client
            .get(format!("{}/addons/self/info", self.origin))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|_| DiscoveryError)?;
        if !response.status().is_success() {
            return Err(DiscoveryError);
        }
        let info: Envelope<AppInfo> = read_json(response).await?;
        // Supervisor provides a DNS hostname, never a user-selected origin.
        if info.result != "ok"
            || info.data.hostname.is_empty()
            || info.data.hostname.len() > 253
            || !info
                .data
                .hostname
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
        {
            return Err(DiscoveryError);
        }
        let response = self
            .client
            .post(format!("{}/discovery", self.origin))
            .bearer_auth(&self.token)
            .json(&Announcement {
                service: "shipments",
                config: Connection {
                    backend_url: format!("http://{}:8080", info.data.hostname),
                    token: integration_token,
                },
            })
            .send()
            .await
            .map_err(|_| DiscoveryError)?;
        if !response.status().is_success() {
            return Err(DiscoveryError);
        }
        #[derive(Deserialize)]
        struct Discovery {
            uuid: String,
        }
        let result: Envelope<Discovery> = read_json(response).await?;
        if result.result != "ok" || result.data.uuid.is_empty() {
            return Err(DiscoveryError);
        }
        Ok(())
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, DiscoveryError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| DiscoveryError)? {
        if body.len() + chunk.len() > 64 * 1024 {
            return Err(DiscoveryError);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| DiscoveryError)
}

#[cfg(test)]
mod tests {
    use crate::supervisor::Supervisor;
    use serde::Deserialize;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    fn server(
        replies: Vec<(u16, String)>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            for (status, body) in replies {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) => panic!("accept request: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                    if bytes.ends_with(b"\r\n\r\n") {
                        break bytes.len();
                    }
                };
                let headers = String::from_utf8(bytes.clone()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                bytes.resize(header_end + length, 0);
                stream.read_exact(&mut bytes[header_end..]).unwrap();
                tx.send(String::from_utf8(bytes).unwrap()).unwrap();
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (origin, rx, worker)
    }

    #[tokio::test]
    async fn only_dedicated_credential_is_announced_to_authenticated_supervisor() {
        #[derive(Deserialize)]
        struct Announcement {
            service: String,
            config: Connection,
        }
        #[derive(Deserialize)]
        struct Connection {
            backend_url: String,
            token: String,
        }
        let (origin, requests, worker) = server(vec![
            (200, r#"{"result":"ok","data":{"hostname":"example-shipments","options":{"easypost_api_key":"private-provider-key"}}}"#.into()),
            (200, r#"{"result":"ok","data":{"uuid":"stable-discovery-id"}}"#.into()),
        ]);
        let supervisor = Supervisor::new("supervisor-only-secret".into(), origin).unwrap();
        supervisor
            .announce("integration-only-secret")
            .await
            .unwrap();
        let info = requests.recv().unwrap();
        assert!(info.starts_with("GET /addons/self/info "));
        assert!(info.contains("authorization: Bearer supervisor-only-secret"));
        let announcement = requests.recv().unwrap();
        assert!(announcement.starts_with("POST /discovery "));
        assert!(announcement.contains("authorization: Bearer supervisor-only-secret"));
        assert!(!announcement.contains("private-provider-key"));
        let body: Announcement =
            serde_json::from_str(announcement.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body.service, "shipments");
        assert_eq!(body.config.backend_url, "http://example-shipments:8080");
        assert_eq!(body.config.token, "integration-only-secret");
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn unsafe_supervisor_responses_are_redacted_and_not_announced() {
        for body in [
            r#"{"result":"ok","data":{"hostname":"host/evil"}}"#,
            r#"{"result":"error","data":{"hostname":"host"}}"#,
            "not-json-secret",
            r#"{"result":"ok","data":{}}"#,
        ] {
            let (origin, requests, worker) = server(vec![(200, body.into())]);
            let supervisor = Supervisor::new("supervisor-secret".into(), origin).unwrap();
            let error = supervisor.announce("integration-secret").await.unwrap_err();
            assert_eq!(
                error.to_string(),
                "Home Assistant discovery is unavailable; retrying"
            );
            assert!(
                requests
                    .recv()
                    .unwrap()
                    .starts_with("GET /addons/self/info ")
            );
            worker.join().unwrap();
        }
    }

    #[tokio::test]
    async fn supervisor_redirect_never_receives_a_credential() {
        let source = TcpListener::bind("127.0.0.1:0").unwrap();
        let sink = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", source.local_addr().unwrap());
        let destination = format!("http://{}/capture", sink.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut stream, _) = source.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            write!(stream, "HTTP/1.1 307 Temporary Redirect\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let supervisor = Supervisor::new("supervisor-secret".into(), origin).unwrap();
        assert!(supervisor.announce("integration-secret").await.is_err());
        sink.set_nonblocking(true).unwrap();
        assert_eq!(
            sink.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        worker.join().unwrap();
    }
}
