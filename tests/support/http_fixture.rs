//! Primitivas de servidor HTTP local compartilhadas pelas integracoes de `slim-core`.
//!
//! Cada binario de teste inclui este modulo com `#[path]`; os prazos de
//! `accept_within` e os limites de `read_http_request` sao os dos fixtures
//! originais, entao os cenarios continuam falhando no mesmo tempo.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

/// Resposta SSE de uma unica mensagem final, sem uso de ferramentas.
pub const SSE_FINAL_ANSWER: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"final answer\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

/// Listener em loopback com porta efemera, ja em modo nao bloqueante.
pub fn bind_listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    (listener, address)
}

/// Aceita uma conexao em `listener` (colocado em modo nao bloqueante) ou
/// entra em panico depois de `deadline`; o stream devolvido e bloqueante.
pub fn accept_within(listener: &TcpListener, deadline: Duration) -> TcpStream {
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let deadline = Instant::now() + deadline;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).expect("blocking stream");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "fixture accept deadline exceeded"
                );
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
}

/// Le uma requisicao HTTP completa (cabecalhos e corpo por Content-Length).
pub fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("request timeout");
    let mut request = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let size = stream.read(&mut chunk).expect("request chunk");
        assert!(size > 0, "request ended before its body was complete");
        request.extend_from_slice(&chunk[..size]);
        assert!(
            request.len() <= 2 * 1024 * 1024,
            "fixture request too large"
        );

        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .expect("content-length header");
        if request.len() >= header_end + 4 + content_length {
            return String::from_utf8(request).expect("utf-8 request");
        }
    }
}

/// Responde 200 `text/event-stream` com `body` e fecha a conexao.
pub fn write_sse(stream: &mut TcpStream, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        )
        .expect("fixture response");
}
