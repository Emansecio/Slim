use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// Keep budget fixtures on localhost through the final, tool-free request.
/// An explicit failure exercises the fallback without waiting for a refused
/// connection after the fixture's first turn has closed its listener.
pub fn reject_budget_finalization(listener: &TcpListener) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "finalization request missing");
                std::thread::yield_now();
            }
            Err(error) => panic!("finalization accept: {error}"),
        }
    };
    stream.set_nonblocking(false).expect("blocking stream");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .expect("write timeout");
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 8192];
    let body = loop {
        let size = stream.read(&mut chunk).expect("finalization request");
        assert!(size > 0, "finalization closed before full body");
        raw.extend_from_slice(&chunk[..size]);
        assert!(raw.len() <= 1024 * 1024, "fixture request exceeded 1 MiB");
        let Some(header_end) = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&raw[..header_end]).expect("headers");
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .expect("content length header");
        let start = header_end + 4;
        if raw.len() >= start + length {
            break serde_json::from_slice::<serde_json::Value>(&raw[start..start + length])
                .expect("finalization JSON");
        }
    };
    assert!(body.get("tools").is_none(), "finalization must omit tools");
    assert!(body.to_string().contains("Budget exhausted"));
    stream
        .write_all(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .expect("explicit finalization failure");
}
