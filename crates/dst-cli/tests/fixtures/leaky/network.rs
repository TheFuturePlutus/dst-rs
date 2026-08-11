// Fixture: NETWORK determinism leaks.

use std::net::{TcpListener, TcpStream, UdpSocket};

pub fn fetch() {
    // LEAK: reqwest::get
    let _ = reqwest::get("http://example.com");
}

pub fn connect() {
    // LEAK: std::net::TcpStream
    let _ = TcpStream::connect("127.0.0.1:8080");
}

pub fn listen() {
    // LEAK: std::net::TcpListener
    let _ = TcpListener::bind("127.0.0.1:0");
}

pub fn udp() {
    // LEAK: std::net::UdpSocket
    let _ = UdpSocket::bind("127.0.0.1:0");
}

pub async fn tokio_connect() {
    // LEAK: tokio::net::TcpStream
    let _ = tokio::net::TcpStream::connect("127.0.0.1:8080").await;
}

// ── Decoys: must NOT be flagged ──

pub fn decoys() {
    // A comment mentioning reqwest::get must not be flagged.
    let net = 3; // a variable named net — NOT a call
    let _ = net + 1;
}
