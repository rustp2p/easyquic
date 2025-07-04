use crate::QuicListener;
use crate::stream::{Map, QuicKey, QuicStream};
use bytes::BytesMut;
use flume::Sender;
use parking_lot::{Mutex, RwLock};
use quiche::{Config, ConnectionId, SendInfo};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

pub fn new_context(config: Config) -> io::Result<(QuicIO, QuicConnect, QuicListener)> {
    let config = Arc::new(Mutex::new(config));
    let (output_sender, output_receiver) = tokio::sync::mpsc::channel(128);
    let (stream_sender, stream_receiver) = flume::bounded(128);
    let map = Arc::new(RwLock::new(HashMap::new()));
    let quic_connect = QuicConnect {
        map: map.clone(),
        output_sender: output_sender.clone(),
        config: config.clone(),
    };

    let output = QuicDataOutput { output_receiver };
    let input = QuicDataInput {
        map,
        config,
        output_sender,
        stream_sender,
    };
    let listener = QuicListener {
        receiver: stream_receiver,
    };
    let quic_io = QuicIO { input, output };
    Ok((quic_io, quic_connect, listener))
}
pub struct QuicIO {
    input: QuicDataInput,
    output: QuicDataOutput,
}
impl QuicIO {
    pub fn split(self) -> (QuicDataInput, QuicDataOutput) {
        (self.input, self.output)
    }
    pub async fn output(&mut self) -> io::Result<(BytesMut, SendInfo)> {
        self.output.output().await
    }
    pub async fn input(
        &mut self,
        data: BytesMut,
        local: SocketAddr,
        remote: SocketAddr,
    ) -> io::Result<()> {
        self.input.input(data, local, remote).await
    }
}
pub struct QuicDataOutput {
    output_receiver: tokio::sync::mpsc::Receiver<(BytesMut, SendInfo)>,
}

pub struct QuicDataInput {
    map: Map,
    config: Arc<Mutex<Config>>,
    output_sender: tokio::sync::mpsc::Sender<(BytesMut, SendInfo)>,
    stream_sender: Sender<QuicStream>,
}
pub struct QuicConnect {
    map: Map,
    output_sender: tokio::sync::mpsc::Sender<(BytesMut, SendInfo)>,
    config: Arc<Mutex<Config>>,
}
impl QuicConnect {
    pub async fn connect(
        &self,
        server_name: Option<&str>,
        local: SocketAddr,
        remote: SocketAddr,
    ) -> io::Result<QuicStream> {
        let rng = SystemRandom::new();
        let mut scid = [0; quiche::MAX_CONN_ID_LEN];
        rng.fill(&mut scid).unwrap();
        let conn_id = ConnectionId::from_ref(&scid);

        let conn = quiche::connect(
            server_name,
            &conn_id,
            local,
            remote,
            &mut self.config.lock(),
        )
        .map_err(io::Error::other)?;
        let quic_key = QuicKey {
            local,
            remote,
            conn_id: conn_id.into_owned(),
        };
        let quic_stream = QuicStream::new(
            quic_key,
            self.map.clone(),
            conn,
            self.output_sender.clone(),
            None,
            true,
        )
        .await?;
        Ok(quic_stream)
    }
}

impl QuicDataOutput {
    pub async fn output(&mut self) -> io::Result<(BytesMut, SendInfo)> {
        self.output_receiver
            .recv()
            .await
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))
    }
}
impl QuicDataInput {
    pub async fn input(
        &mut self,
        mut data: BytesMut,
        local: SocketAddr,
        remote: SocketAddr,
    ) -> io::Result<()> {
        let hdr = quiche::Header::from_slice(&mut data, quiche::MAX_CONN_ID_LEN)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let option = self
            .map
            .read()
            .get(&QuicKey::new(local, remote, hdr.scid.into_owned()))
            .cloned();
        if let Some(sender) = option {
            // Ignore disconnection exceptions
            _ = sender.send(data).await;
            return Ok(());
        }

        let option = self
            .map
            .read()
            .get(&QuicKey::new(local, remote, hdr.dcid.into_owned()))
            .cloned();
        if let Some(sender) = option {
            // Ignore disconnection exceptions
            _ = sender.send(data).await;
            return Ok(());
        }

        // new quic connect
        if hdr.ty == quiche::Type::Initial {
            let mut scid = [0; quiche::MAX_CONN_ID_LEN];
            SystemRandom::new().fill(&mut scid).unwrap();
            let scid = ConnectionId::from_ref(&scid);
            let conn = quiche::accept(&scid, None, local, remote, &mut self.config.lock())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let quic_key = QuicKey::new(local, remote, scid.into_owned());
            let quic_stream = QuicStream::new(
                quic_key,
                self.map.clone(),
                conn,
                self.output_sender.clone(),
                Some(data),
                false,
            )
            .await?;
            self.stream_sender
                .send_async(quic_stream)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        }
        Ok(())
    }
}
