use crate::stream::QuicStream;
use flume::Receiver;
use std::io;
use std::net::SocketAddr;

pub struct QuicListener {
    pub(crate) receiver: Receiver<QuicStream>,
}
impl QuicListener {
    pub async fn accept(&self) -> io::Result<(QuicStream, SocketAddr)> {
        let quic_stream = self
            .receiver
            .recv_async()
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::Other))?;
        let socket_addr = quic_stream.remote_addr()?;
        Ok((quic_stream, socket_addr))
    }
}
