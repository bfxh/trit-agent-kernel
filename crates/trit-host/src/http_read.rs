//! 参考服务端与账本服务端共享的 HTTP 请求读取边界。
//!
//! 两个服务端都是最小 HTTP/1.1 实现：一连接一请求、自己解析请求行与头部。这里把
//! 行长度、头数量、总头字节和请求读取总时限集中在一处，避免两个服务端的资源边界
//! 各自漂移。

use std::io::{self, BufRead, Read};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// 连接上的读写超时：防止半开的客户端长期占住服务线程。
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// 请求读取的默认总时限：覆盖请求行、头部与请求体；0 表示关闭。
pub(crate) const DEFAULT_REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// HTTP 请求行/单个头部的字节上限；超限直接 431，不继续缓冲。
pub(crate) const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// 整个请求行 + 头部块的总字节上限。
pub(crate) const MAX_HEADER_BYTES: usize = 64 * 1024;

/// 头部字段数量上限（含请求行之外的空行）。
pub(crate) const MAX_HEADERS: usize = 64;

#[derive(Debug)]
pub(crate) enum HeaderReadError {
    Io(io::Error),
    TooLarge,
    Invalid,
}

/// 有界读取一行 HTTP 头：逐块从 `BufReader` 取，不先分配整行。
pub(crate) fn read_http_line<R: BufRead>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<String>, HeaderReadError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(HeaderReadError::Io)?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_| HeaderReadError::Invalid)
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        if bytes.len().saturating_add(take) > limit {
            reader.consume(take);
            return Err(HeaderReadError::TooLarge);
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| HeaderReadError::Invalid);
        }
    }
}

/// socket 读写超时与请求读取总时限都会以这两种错误暴露。
pub(crate) fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

/// 提前拒绝（431/408 等）后的收尾：先 shutdown 写端，再把对端仍在发送的字节
/// 读干净，最后才关闭 socket。
///
/// 直接关闭还有未读数据的连接会触发 RST：客户端可能还没读到响应就被
/// `ConnectionReset`，在 macOS 上尤其容易复现。`respond` 之后调用本函数，
/// 让响应先到达客户端。
pub(crate) fn finish_early_rejection(stream: &mut TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut sink = [0u8; 4096];
    let mut drained = 0usize;
    while drained < MAX_HEADER_BYTES {
        match stream.read(&mut sink) {
            Ok(0) => break,
            Ok(bytes) => drained = drained.saturating_add(bytes),
            Err(_) => break,
        }
    }
}

/// 给底层 socket 的每次读取加上绝对截止点。
///
/// 仅靠 `set_read_timeout` 只能约束“多久没有数据”；不断发一个字节的慢速客户端
/// 会让每次读取都成功。这里在每次底层读取前按剩余时间重设超时，并先检查总截止点。
pub(crate) struct DeadlineStream {
    stream: TcpStream,
    deadline: Option<Instant>,
}

impl DeadlineStream {
    pub(crate) fn new(stream: TcpStream, timeout: Option<Duration>) -> io::Result<Self> {
        let deadline = match timeout {
            Some(timeout) => Some(Instant::now().checked_add(timeout).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "request read timeout is too large",
                )
            })?),
            None => None,
        };
        Ok(Self { stream, deadline })
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "request read deadline exceeded",
                ));
            }
            let timeout = remaining.min(IO_TIMEOUT).max(Duration::from_millis(1));
            self.stream.set_read_timeout(Some(timeout))?;
        }
        self.stream.read(buf)
    }
}
