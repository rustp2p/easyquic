# easyquic

**easyquic** is an out-of-the-box, ergonomic QUIC (Quick UDP Internet Connections) library for Rust, focusing on ease of use and seamless integration with async ecosystems like Tokio.  
It is designed to let you build reliable, high-performance, low-latency UDP-based applications with minimal setup and maximum flexibility.

## Features

- Simple and high-level QUIC API
- Easy integration with async/await (Tokio-based)
- Built-in connection and stream management
- Full `quiche`-based implementation, supporting custom config

## Install

Add to your `Cargo.toml`:

```toml
easyquic = "0.1"
```
## Example
Below is a minimal client example using easyquic, sending and receiving messages over QUIC.
This demo uses Tokio, bytes, and tokio_util::codec for framed messaging.

```rust
use bytes::{Bytes, BytesMut};
use env_logger::Env;
use futures::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

#[tokio::main]
pub async fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("debug")).init();

    let config = configure_quiche().unwrap();
    let (mut io, conn, _listener) = easyquic::new_context(config).unwrap();
    let local_addr = "0.0.0.0:12346".parse::<SocketAddr>().unwrap();
    let server_addr = "127.0.0.1:12345".parse::<SocketAddr>().unwrap();
    let udp = UdpSocket::bind(local_addr).await.unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            tokio::select! {
                rs = udp.recv_from(&mut buf) => {
                    match rs {
                        Ok((len, remote_addr)) => {
                            io.input(BytesMut::from(&buf[..len]), local_addr, remote_addr).await.unwrap();
                        }
                        Err(e) => {
                            println!("Error receiving UDP packet: {e}");
                        }
                    }
                }
                output = io.output() => {
                    let (buf, send_info) = output.unwrap();
                    if let Err(e) = udp.send_to(&buf, send_info.to).await {
                        println!("Error sending UDP message {e}");
                    }
                }
            }
        }
    });
    let stream = conn.connect(None, local_addr, server_addr).await.unwrap();
    println!("quic connection from {server_addr}");
    let mut framed = Framed::new(stream, LengthDelimitedCodec::new());
    println!("quic sending message");
    framed.send(Bytes::from("hello")).await.unwrap();
    let rs = framed.next().await.unwrap().unwrap();
    println!("recv message {:?}", std::str::from_utf8(&rs));
}

pub fn configure_quiche() -> Result<quiche::Config, Box<dyn std::error::Error>> {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;

    config.set_application_protos(&[b"hello"])?;
    config.set_max_idle_timeout(5000);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.verify_peer(false);
    Ok(config)
}
 
```