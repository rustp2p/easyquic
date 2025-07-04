use bytes::{Bytes, BytesMut};
use clap::Parser;
use env_logger::Env;
use futures::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    server: SocketAddr,
}
#[tokio::main]
pub async fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("debug")).init();

    let args = Args::parse();
    let config = configure_quiche().unwrap();
    let (mut io, conn, _listener) = easyquic::new_context(config).unwrap();
    let local_addr = "0.0.0.0:12346".parse::<SocketAddr>().unwrap();
    let server_addr = args.server;
    let udp = UdpSocket::bind(local_addr).await.unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            tokio::select! {
                rs=udp.recv_from(&mut buf) => {
                    match rs {
                        Ok((len,remote_addr)) => {
                            io.input(BytesMut::from(&buf[..len]),local_addr,remote_addr).await.unwrap();
                        }
                        Err(e) => {
                            println!("Error receiving UDP packet: {e}");
                        }
                    }
                }
                output=io.output() => {
                    let (buf,send_info) = output.unwrap();
                    if let Err(e) = udp.send_to(&buf,send_info.to).await{
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
