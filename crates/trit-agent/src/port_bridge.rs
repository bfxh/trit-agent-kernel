//! 端口级零停机桥接。
//!
//! 前端监听口由 bridge 持有，跨后端进程重启保持打开；新连接在后端不可达时
//! 在超时窗口内重试，而不是立刻收到 connection refused。已建立的连接仍会随
//! 后端进程退出而断开——这不是内核级 FD 交接或连接迁移。

use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

fn default_connect_timeout_ms() -> u64 {
    30_000
}

fn default_retry_ms() -> u64 {
    50
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortBridgeSpec {
    pub name: String,
    pub listen: String,
    pub target: String,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_retry_ms")]
    pub retry_ms: u64,
}

impl PortBridgeSpec {
    pub fn new(
        name: impl Into<String>,
        listen: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            listen: listen.into(),
            target: target.into(),
            connect_timeout_ms: default_connect_timeout_ms(),
            retry_ms: default_retry_ms(),
        }
    }
}

pub struct PortBridge {
    name: String,
    listen: SocketAddr,
    target: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PortBridge {
    pub fn start(spec: &PortBridgeSpec) -> io::Result<Self> {
        let listen = resolve(&spec.listen)?;
        let target = resolve(&spec.target)?;
        let listener = TcpListener::bind(listen)?;
        listener.set_nonblocking(true)?;
        let listen = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let timeout = Duration::from_millis(spec.connect_timeout_ms.max(1));
        let retry = Duration::from_millis(spec.retry_ms.max(1));
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let stop = Arc::clone(&thread_stop);
                        thread::spawn(move || forward(client, target, timeout, retry, stop));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(50)),
                }
            }
        });
        Ok(Self {
            name: spec.name.clone(),
            listen,
            target,
            stop,
            handle: Some(handle),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listen
    }

    pub fn target_addr(&self) -> SocketAddr {
        self.target
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PortBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

fn resolve(address: &str) -> io::Result<SocketAddr> {
    address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "address resolves to nothing"))
}

fn forward(
    mut client: TcpStream,
    target: SocketAddr,
    timeout: Duration,
    retry: Duration,
    stop: Arc<AtomicBool>,
) {
    let deadline = Instant::now() + timeout;
    let mut backend = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || stop.load(Ordering::Relaxed) {
            let _ = client.shutdown(Shutdown::Both);
            return;
        }
        match TcpStream::connect_timeout(&target, remaining.min(Duration::from_millis(500))) {
            Ok(stream) => break stream,
            Err(_) => thread::sleep(retry.min(remaining)),
        }
    };
    let _ = client.set_nodelay(true);
    let _ = backend.set_nodelay(true);
    let (mut client_read, mut backend_write) = match (client.try_clone(), backend.try_clone()) {
        (Ok(client_read), Ok(backend_write)) => (client_read, backend_write),
        _ => {
            let _ = client.shutdown(Shutdown::Both);
            let _ = backend.shutdown(Shutdown::Both);
            return;
        }
    };
    let upstream = thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut backend_write);
        let _ = backend_write.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut backend, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = upstream.join();
    let _ = client.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    fn free_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    fn echo_once(listener: TcpListener) -> JoinHandle<()> {
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 16];
            let read = stream.read(&mut buffer).unwrap();
            stream.write_all(&buffer[..read]).unwrap();
        })
    }

    #[test]
    fn front_port_stays_open_while_backend_is_absent() {
        let target = free_addr();
        let bridge = PortBridge::start(&PortBridgeSpec {
            name: "gateway".into(),
            listen: "127.0.0.1:0".into(),
            target: target.to_string(),
            connect_timeout_ms: 20_000,
            retry_ms: 20,
        })
        .unwrap();
        let front = bridge.local_addr();

        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(front).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            stream.write_all(b"ping").unwrap();
            let mut buffer = [0u8; 4];
            stream.read_exact(&mut buffer).unwrap();
            buffer
        });
        thread::sleep(Duration::from_millis(150));
        let listener = TcpListener::bind(target).unwrap();
        let echo = echo_once(listener);
        assert_eq!(&client.join().unwrap(), b"ping");
        echo.join().unwrap();
        drop(bridge);
    }
}
