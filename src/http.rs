// SPDX-License-Identifier: MIT
//! Just enough HTTP/1.1 for the daemon and the GUI's remote mode, which only
//! talk to each other: one request per connection (Connection: close), bodies
//! sized by Content-Length, and every size and wait bounded. httparse reads the
//! heads.

use std::io::{self, Read, Write};
#[cfg(any(target_os = "linux", test))]
use serde::Serialize;
// The client side is the GUI's.
#[cfg(any(feature = "gui", test))]
use {
    serde::de::DeserializeOwned,
    std::net::{TcpStream, ToSocketAddrs},
    std::time::Duration,
};

/// The most a request's or response's head may take.
const MAX_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 32;
/// The most a response body may take: a whole flash image, with room to spare.
#[cfg(any(feature = "gui", test))]
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

// The server side is the daemon's.
#[cfg(any(target_os = "linux", test))]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[cfg(any(target_os = "linux", test))]
impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// A numeric parameter; `default` when it is absent.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn num<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.param(name) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|_| format!("bad {name}: {v}")),
        }
    }

    /// A 0/1 parameter.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn flag(&self, name: &str) -> bool {
        self.param(name) == Some("1")
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    #[cfg(any(target_os = "linux", test))]
    pub fn new(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self { status, headers: vec![("Content-Type".into(), content_type.into())], body }
    }

    #[cfg(any(target_os = "linux", test))]
    pub fn json<T: Serialize + ?Sized>(status: u16, value: &T) -> Self {
        Self::new(status, "application/json", serde_json::to_vec(value).unwrap_or_default())
    }

    /// An error, as {"error": "..."}.
    #[cfg(any(target_os = "linux", test))]
    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, &serde_json::json!({ "error": message.into() }))
    }

    #[cfg(any(target_os = "linux", test))]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    #[cfg(any(feature = "gui", test))]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    #[cfg(any(feature = "gui", test))]
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as JSON, or what went wrong.
    #[cfg(any(feature = "gui", test))]
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, String> {
        if !self.ok() {
            return Err(self.message());
        }
        serde_json::from_slice(&self.body).map_err(|e| format!("unexpected answer: {e}"))
    }

    /// The error a failed response carries.
    #[cfg(any(feature = "gui", test))]
    pub fn message(&self) -> String {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.get("error")?.as_str().map(str::to_string))
            .unwrap_or_else(|| format!("HTTP {} {}", self.status, reason(self.status)))
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

fn invalid(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

/// Reads to the end of a head; returns it and whatever of the body came with it.
fn read_head(r: &mut impl Read) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf[from..].windows(4).position(|w| w == b"\r\n\r\n") {
            let end = from + i + 4;
            let rest = buf.split_off(end);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEAD {
            return Err(invalid("head too large"));
        }
    }
}

fn read_body(r: &mut impl Read, mut body: Vec<u8>, len: usize) -> io::Result<Vec<u8>> {
    if body.len() > len {
        body.truncate(len);
    }
    body.reserve_exact(len - body.len());
    let mut chunk = [0u8; 64 * 1024];
    while body.len() < len {
        let n = r.read(&mut chunk[..(len - body.len()).min(64 * 1024)])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(body)
}

fn content_length(headers: &[httparse::Header]) -> io::Result<usize> {
    if headers.iter().any(|h| h.name.eq_ignore_ascii_case("transfer-encoding")) {
        return Err(invalid("chunked bodies are not supported"));
    }
    match headers.iter().find(|h| h.name.eq_ignore_ascii_case("content-length")) {
        None => Ok(0),
        Some(h) => std::str::from_utf8(h.value).ok().and_then(|v| v.trim().parse().ok()).ok_or_else(|| invalid("bad Content-Length")),
    }
}

#[cfg(any(target_os = "linux", test))]
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() && hex(b[i + 1]).is_some() && hex(b[i + 2]).is_some() => {
                out.push(hex(b[i + 1]).unwrap() << 4 | hex(b[i + 2]).unwrap());
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads one request; `max_body` says how large a body this method and path may have.
#[cfg(any(target_os = "linux", test))]
pub fn read_request(r: &mut impl Read, max_body: impl Fn(&str, &str) -> usize) -> io::Result<Request> {
    let (head, rest) = read_head(r)?;
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(&head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(invalid("bad request head")),
    }
    let (method, target) = (req.method.unwrap_or("").to_string(), req.path.unwrap_or("/"));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (unescape(k), unescape(v))
        })
        .collect();
    let len = content_length(req.headers)?;
    if len > max_body(&method, path) {
        return Err(io::Error::new(io::ErrorKind::OutOfMemory, "body too large"));
    }
    let headers = req.headers.iter().map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).into_owned())).collect();
    let path = unescape(path);
    let body = read_body(r, rest, len)?;
    Ok(Request { method, path, query, headers, body })
}

#[cfg(any(target_os = "linux", test))]
pub fn write_response(w: &mut impl Write, resp: &Response) -> io::Result<()> {
    let mut head =
        format!("HTTP/1.1 {} {}\r\nConnection: close\r\nContent-Length: {}\r\n", resp.status, reason(resp.status), resp.body.len());
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(&resp.body)?;
    w.flush()
}

/// What a failed connection means, for people.
#[cfg(any(feature = "gui", test))]
fn said(host: &str, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => format!("{host} refused the connection; is k1cam-flasher daemon running there?"),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => format!("{host} did not answer in time"),
        io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof => format!("{host} closed the connection"),
        _ => format!("{host}: {e}"),
    }
}

/// One request to `host` (host:port); the answer must start within `wait`.
#[cfg(any(feature = "gui", test))]
pub fn call(host: &str, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8], wait: Duration) -> Result<Response, String> {
    let addrs: Vec<_> = host.to_socket_addrs().map_err(|e| format!("{host}: {e}"))?.collect();
    let mut last = format!("{host}: no address");
    let mut stream = None;
    for a in addrs {
        match TcpStream::connect_timeout(&a, Duration::from_secs(3)) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = said(host, &e),
        }
    }
    let mut s = stream.ok_or(last)?;
    let _ = s.set_nodelay(true);
    s.set_write_timeout(Some(Duration::from_secs(30))).map_err(|e| said(host, &e))?;
    s.set_read_timeout(Some(wait)).map_err(|e| said(host, &e))?;
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    if body.len() <= 64 * 1024 {
        out.extend_from_slice(body);
        s.write_all(&out).map_err(|e| said(host, &e))?;
    } else {
        s.write_all(&out).and_then(|_| s.write_all(body)).map_err(|e| said(host, &e))?;
    }
    read_response(&mut s).map_err(|e| said(host, &e))
}

#[cfg(any(feature = "gui", test))]
pub fn read_response(r: &mut impl Read) -> io::Result<Response> {
    let (head, rest) = read_head(r)?;
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut resp = httparse::Response::new(&mut headers);
    match resp.parse(&head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(invalid("bad response head")),
    }
    let status = resp.code.unwrap_or(0);
    let len = content_length(resp.headers)?;
    if len > MAX_RESPONSE {
        return Err(invalid("response too large"));
    }
    let headers = resp.headers.iter().map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).into_owned())).collect();
    let body = read_body(r, rest, len)?;
    Ok(Response { status, headers, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        let raw = b"POST /api/v1/jobs?op=flash&id=a-1&name=k1%20cam HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nContent-Length: 5\r\n\r\nhello";
        let r = read_request(&mut &raw[..], |_, _| 16).unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/api/v1/jobs"));
        assert_eq!((r.param("op"), r.param("id"), r.param("name")), (Some("flash"), Some("a-1"), Some("k1 cam")));
        assert_eq!(r.header("authorization"), Some("Bearer t"));
        assert_eq!(r.body, b"hello");
        // Bodies over the limit are refused before they are read.
        let e = read_request(&mut &raw[..], |_, _| 4).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::OutOfMemory);
        // So are chunked ones, and endless heads.
        let chunked = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(read_request(&mut &chunked[..], |_, _| 16).is_err());
        let endless = vec![b'a'; MAX_HEAD + 8192];
        assert!(read_request(&mut &endless[..], |_, _| 16).is_err());
    }

    #[test]
    fn responses_round_trip() {
        let mut out = Vec::new();
        write_response(&mut out, &Response::error(409, "busy").header("K1cam-Seq", "7")).unwrap();
        let r = read_response(&mut &out[..]).unwrap();
        assert_eq!((r.status, r.get("k1cam-seq"), r.message().as_str()), (409, Some("7"), "busy"));
        assert!(r.parse::<serde_json::Value>().is_err());
        let mut out = Vec::new();
        write_response(&mut out, &Response::json(200, &[1, 2])).unwrap();
        assert_eq!(read_response(&mut &out[..]).unwrap().parse::<Vec<u8>>().unwrap(), [1, 2]);
    }

    #[test]
    fn unescapes() {
        assert_eq!(unescape("a%20b%2F%C3%A7+%26"), "a b/ç &");
        assert_eq!(unescape("100%"), "100%");
        assert_eq!(unescape("%zz%4"), "%zz%4");
    }
}
