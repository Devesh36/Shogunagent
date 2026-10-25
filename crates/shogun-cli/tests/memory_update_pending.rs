//! #800: a headless REST face refusing memory with 503 `memory_update_pending` must reach the CLI
//! user as that code and sentence, with a failing exit status.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;

use shogun_mcp::memory_readiness::{MemoryUnavailable, UPDATE_PENDING_MESSAGE};

#[test]
fn a_503_memory_update_pending_surfaces_the_code_and_message_and_fails() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).unwrap();
        let body = MemoryUnavailable::UpdatePending.rest_body();
        let reply = format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(reply.as_bytes()).unwrap();
    });

    let out = Command::new(env!("CARGO_BIN_EXE_shogun"))
        .args(["search", "forecast"])
        .env("SHOGUN_API_PORT", port.to_string())
        .env("SHOGUN_API_TOKEN", "tok")
        .output()
        .unwrap();
    server.join().unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("memory_update_pending"), "{stdout}");
    assert!(stdout.contains(UPDATE_PENDING_MESSAGE), "{stdout}");
    assert_eq!(out.status.code(), Some(1), "an HTTP error status exits 1");
}
