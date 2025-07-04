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
    let (mut io, _conn, listener) = easyquic::new_context(config).unwrap();
    let local_addr = "0.0.0.0:12345".parse::<SocketAddr>().unwrap();
    let udp = UdpSocket::bind(local_addr).await.unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            tokio::select! {
                rs=udp.recv_from(&mut buf) => {
                    let (len,remote_addr) = rs.unwrap();
                    io.input(BytesMut::from(&buf[..len]),local_addr,remote_addr).await.unwrap();
                }
                output=io.output() => {
                    let (buf,send_info) = output.unwrap();
                    udp.send_to(&buf,send_info.to).await.unwrap();
                }
            }
        }
    });
    println!("quic listening... {local_addr}");
    loop {
        let (stream, addr) = listener.accept().await.unwrap();
        log::info!("accept connection from {addr}");
        let mut framed = Framed::new(stream, LengthDelimitedCodec::new());
        tokio::spawn(async move {
            while let Some(frame) = framed.next().await {
                match frame {
                    Ok(buf) => {
                        println!(
                            "Received a frame from {addr:?},{:?}",
                            std::str::from_utf8(&buf[..])
                        );
                        if let Err(e) = framed.send(Bytes::from(format!("hello {addr}"))).await {
                            println!("Error sending frame from {addr:?},{e:?}");
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("connection error: {e}");
                        break;
                    }
                }
            }
            println!("Connection closed {addr}");
        });
    }
}

pub fn configure_quiche() -> Result<quiche::Config, Box<dyn std::error::Error>> {
    pub const MAX_DATAGRAM_SIZE: usize = 1350;
    pub const MAX_DATA: u64 = 10_000_000;
    pub const MAX_STREAM_DATA: u64 = 1_000_000;
    pub const MAX_STREAMS_BIDI: u64 = 100;
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;

    let subject_alt_names = vec!["localhost".to_string()];

    let cert = rcgen::generate_simple_self_signed(subject_alt_names)?;
    let cert_pem = cert.cert.pem();
    let key_pem = cert.signing_key.serialize_pem();
    println!("{}", cert_pem);
    println!("{}", key_pem);

    std::fs::write("cert.pem", &cert_pem)?;
    std::fs::write("key.pem", &key_pem)?;

    config.set_application_protos(&[b"hello"])?;
    config.set_max_idle_timeout(5000);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(MAX_DATA);
    config.set_initial_max_stream_data_bidi_local(MAX_STREAM_DATA);
    config.set_initial_max_stream_data_bidi_remote(MAX_STREAM_DATA);
    config.set_initial_max_streams_bidi(MAX_STREAMS_BIDI);
    config.set_disable_active_migration(true);
    config.set_cc_algorithm(quiche::CongestionControlAlgorithm::CUBIC);
    config.load_cert_chain_from_pem_file("cert.pem")?;
    config.load_priv_key_from_pem_file("key.pem")?;
    Ok(config)
}
