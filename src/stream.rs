use bytes::{Buf, BytesMut};
use parking_lot::RwLock;
use quiche::{Connection, ConnectionId, RecvInfo, SendInfo};
use std::collections::HashMap;
use std::io;
use std::io::Error;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::Sleep;
use tokio_util::sync::PollSender;

pub(crate) type Map = Arc<RwLock<HashMap<QuicKey, Sender<BytesMut>>>>;
pub struct QuicStream {
    read: QuicStreamRead,
    write: QuicStreamWrite,
}
pub struct QuicStreamRead {
    #[allow(dead_code)]
    owned_quic: Arc<OwnedQuic>,
    last_buf: Option<BytesMut>,
    receiver: Receiver<BytesMut>,
}
pub struct QuicStreamWrite {
    #[allow(dead_code)]
    owned_quic: Arc<OwnedQuic>,
    sender: PollSender<BytesMut>,
}
impl QuicStream {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.read.owned_quic.key.local)
    }
    pub fn remote_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.read.owned_quic.key.remote)
    }
    pub fn id(&self) -> ConnectionId {
        self.read.owned_quic.key.conn_id.clone()
    }
    pub fn split(self) -> (QuicStreamRead, QuicStreamWrite) {
        (self.read, self.write)
    }
}
impl QuicStreamRead {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.owned_quic.key.local)
    }
    pub fn remote_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.owned_quic.key.remote)
    }
    pub fn id(&self) -> ConnectionId {
        self.owned_quic.key.conn_id.clone()
    }
}
impl QuicStreamWrite {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.owned_quic.key.local)
    }
    pub fn remote_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.owned_quic.key.remote)
    }
    pub fn id(&self) -> ConnectionId {
        self.owned_quic.key.conn_id.clone()
    }
}
impl AsyncWrite for QuicStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        Pin::new(&mut self.write).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.write).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.write).poll_shutdown(cx)
    }
}
impl AsyncRead for QuicStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.read).poll_read(cx, buf)
    }
}
impl AsyncRead for QuicStreamRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(p) = self.last_buf.as_mut() {
            let len = buf.remaining().min(p.len());
            buf.put_slice(&p[..len]);
            p.advance(len);
            if p.is_empty() {
                self.last_buf.take();
            }
            return Poll::Ready(Ok(()));
        }
        let poll = self.receiver.poll_recv(cx);
        match poll {
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Ready(Some(mut p)) => {
                if p.is_empty() {
                    self.receiver.close();
                    return Poll::Ready(Ok(()));
                }
                let len = buf.remaining().min(p.len());
                buf.put_slice(&p[..len]);
                p.advance(len);
                if !p.is_empty() {
                    self.last_buf.replace(p);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
impl AsyncWrite for QuicStreamWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        match self.sender.poll_reserve(cx) {
            Poll::Ready(Ok(_)) => {
                let len = buf.len().min(10240);
                match self.sender.send_item(buf[..len].into()) {
                    Ok(_) => Poll::Ready(Ok(len)),
                    Err(_) => Poll::Ready(Err(io::Error::from(io::ErrorKind::WriteZero))),
                }
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::from(io::ErrorKind::WriteZero))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.sender.close();
        Poll::Ready(Ok(()))
    }
}

impl QuicStream {
    pub(crate) async fn new(
        quic_key: QuicKey,
        map: Map,
        quic_conn: Connection,
        output: Sender<(BytesMut, SendInfo)>,
        data: Option<BytesMut>,
        is_wait_established: bool,
    ) -> io::Result<Self> {
        let (owned_quic, input_receiver) = {
            let mut guard = map.write();
            if guard.contains_key(&quic_key) {
                return Err(Error::new(
                    io::ErrorKind::AlreadyExists,
                    "stream already exists",
                ));
            }
            let (input_sender, input_receiver) = tokio::sync::mpsc::channel(128);
            if let Some(data) = data {
                _ = input_sender.try_send(data);
            }
            guard.insert(quic_key.clone(), input_sender);
            drop(guard);
            let owned_quic = OwnedQuic { key: quic_key, map };
            (owned_quic, input_receiver)
        };
        QuicStream::new_stream(
            owned_quic,
            quic_conn,
            input_receiver,
            output,
            is_wait_established,
        )
        .await
    }
    async fn new_stream(
        owned_quic: OwnedQuic,
        mut quic_conn: Connection,
        mut input: Receiver<BytesMut>,
        output: Sender<(BytesMut, SendInfo)>,
        is_wait_established: bool,
    ) -> io::Result<Self> {
        let (data_in_sender, data_in_receiver) = tokio::sync::mpsc::channel(128);
        let (data_out_sender, data_out_receiver) = tokio::sync::mpsc::channel(128);
        let recv_info = owned_quic.key.recv_info();
        if is_wait_established {
            wait_established(recv_info, &mut quic_conn, &mut input, &output).await?;
        }
        tokio::spawn(async move {
            if let Err(e) = quic_run(
                recv_info,
                input,
                output,
                quic_conn,
                data_out_receiver,
                data_in_sender,
            )
            .await
            {
                log::debug!("quic run: {e:?},recv_info: {recv_info:?}");
            }
        });
        let owned_quic = Arc::new(owned_quic);
        let read = QuicStreamRead {
            owned_quic: owned_quic.clone(),
            last_buf: None,
            receiver: data_in_receiver,
        };
        let write = QuicStreamWrite {
            owned_quic,
            sender: PollSender::new(data_out_sender),
        };
        Ok(QuicStream { read, write })
    }
}
#[derive(Clone, Eq, PartialEq, Hash, Debug)]
pub(crate) struct QuicKey {
    pub(crate) local: SocketAddr,
    pub(crate) remote: SocketAddr,
    pub(crate) conn_id: ConnectionId<'static>,
}
impl QuicKey {
    pub(crate) fn new(
        local: SocketAddr,
        remote: SocketAddr,
        conn_id: ConnectionId<'static>,
    ) -> Self {
        Self {
            local,
            remote,
            conn_id,
        }
    }
    pub(crate) fn recv_info(&self) -> RecvInfo {
        RecvInfo {
            from: self.remote,
            to: self.local,
        }
    }
}
struct OwnedQuic {
    key: QuicKey,
    map: Map,
}
impl Drop for OwnedQuic {
    fn drop(&mut self) {
        let mut guard = self.map.write();
        let _ = guard.remove(&self.key);
    }
}
pub(crate) async fn wait_established(
    recv_info: RecvInfo,
    quic_conn: &mut Connection,
    input: &mut Receiver<BytesMut>,
    output: &Sender<(BytesMut, SendInfo)>,
) -> io::Result<()> {
    let mut buf = vec![0; 65536];
    let mut sleep_until = quic_conn
        .timeout_instant()
        .map(|time| Box::pin(tokio::time::sleep_until(time.into())));

    loop {
        let event = if let Some(timeout) = sleep_until.as_mut() {
            input_event(input, timeout).await?
        } else {
            match input.try_recv() {
                Ok(rs) => Event::Input(rs),
                Err(e) => match e {
                    TryRecvError::Empty => Event::Timeout,
                    TryRecvError::Disconnected => {
                        return Err(io::Error::from(io::ErrorKind::BrokenPipe));
                    }
                },
            }
        };
        if quic_conn.is_closed() {
            return Err(Error::from(io::ErrorKind::ConnectionAborted));
        }
        match event {
            Event::Output(_) => {}
            Event::Input(mut buf) => {
                match quic_conn.recv(&mut buf, recv_info) {
                    Ok(_) => {}
                    Err(quiche::Error::Done) => {}
                    Err(e) => {
                        return Err(Error::other(e));
                    }
                };
            }
            Event::Timeout => {
                quic_conn.on_timeout();
                sleep_until = quic_conn
                    .timeout_instant()
                    .map(|time| Box::pin(tokio::time::sleep_until(time.into())));
            }
        }
        if quic_conn.is_established() {
            break;
        }
        loop {
            match quic_conn.send(&mut buf) {
                Ok((len, info)) => {
                    if output
                        .send((BytesMut::from(&buf[..len]), info))
                        .await
                        .is_err()
                    {
                        return Err(Error::from(io::ErrorKind::WriteZero));
                    }
                }
                Err(quiche::Error::Done) => {
                    break;
                }
                Err(e) => {
                    return Err(Error::other(e));
                }
            }
        }
    }
    Ok(())
}
pub(crate) async fn quic_run(
    recv_info: RecvInfo,
    mut input: Receiver<BytesMut>,
    output: Sender<(BytesMut, SendInfo)>,
    mut quic_conn: Connection,
    mut data_out_receiver: Receiver<BytesMut>,
    data_in_sender: Sender<BytesMut>,
) -> io::Result<()> {
    let mut sleep_until = if let Some(time) = quic_conn.timeout_instant() {
        Box::pin(tokio::time::sleep_until(time.into()))
    } else {
        Box::pin(tokio::time::sleep(Duration::from_millis(10)))
    };
    let mut buf = vec![0; 65536];
    let mut input_data = None::<BytesMut>;
    let mut output_data = None::<BytesMut>;
    'out: loop {
        if quic_conn.is_closed() {
            log::debug!("quic connection closed:{recv_info:?}");
            break;
        }
        loop {
            match quic_conn.send(&mut buf) {
                Ok((len, info)) => {
                    if output
                        .send((BytesMut::from(&buf[..len]), info))
                        .await
                        .is_err()
                    {
                        log::debug!("quic send failed,recv_info={recv_info:?}");
                        break 'out;
                    }
                }
                Err(quiche::Error::Done) => {
                    break;
                }
                Err(e) => {
                    log::debug!("Error quic output: {e},recv_info={recv_info:?}");
                    break 'out;
                }
            }
        }

        let event =
            if input_data.is_none() && (output_data.is_some() || !quic_conn.is_established()) {
                input_event(&mut input, &mut sleep_until).await?
            } else if input_data.is_some() {
                output_event(&mut data_out_receiver, &mut sleep_until).await?
            } else {
                all_event(&mut input, &mut data_out_receiver, &mut sleep_until).await?
            };
        if let Some(mut buf) = input_data.take() {
            let len = match quic_conn.recv(&mut buf, recv_info) {
                Ok(len) => len,
                Err(quiche::Error::Done) => buf.len(),
                Err(e) => {
                    log::debug!("Error quic input recv: {e},recv_info={recv_info:?}");
                    break 'out;
                }
            };
            if len < buf.len() {
                buf.advance(len);
                input_data.replace(buf);
            }
        }
        if let Some(mut buf) = output_data.take() {
            let len = match quic_conn.stream_send(0, &buf, false) {
                Ok(len) => len,
                Err(quiche::Error::Done) => buf.len(),
                Err(e) => {
                    log::debug!("Error writing to quic stream: {e},recv_info={recv_info:?}");
                    break 'out;
                }
            };
            if len < buf.len() {
                buf.advance(len);
                output_data.replace(buf);
            }
        }
        match event {
            Event::Input(mut buf) => {
                let len = match quic_conn.recv(&mut buf, recv_info) {
                    Ok(len) => len,
                    Err(e) => {
                        log::debug!("Error quic input: {e},recv_info={recv_info:?}");
                        break 'out;
                    }
                };
                if len < buf.len() {
                    buf.advance(len);
                    input_data.replace(buf);
                }
            }
            Event::Output(mut buf) => match quic_conn.stream_send(0, &buf, buf.is_empty()) {
                Ok(len) => {
                    if len < buf.len() {
                        buf.advance(len);
                        output_data.replace(buf);
                    }
                }
                Err(quiche::Error::Done) => {}
                Err(e) => {
                    log::warn!("Error writing to quic stream: {e:?},recv_info={recv_info:?}");
                    break 'out;
                }
            },
            Event::Timeout => {
                quic_conn.on_timeout();
                if let Some(time) = quic_conn.timeout_instant() {
                    sleep_until = Box::pin(tokio::time::sleep_until(time.into()))
                } else {
                    sleep_until = Box::pin(tokio::time::sleep(Duration::from_millis(10)))
                }
            }
        }
        loop {
            if !quic_conn.stream_readable(0) {
                break;
            }
            match quic_conn.stream_recv(0, &mut buf) {
                Ok((len, fin)) => {
                    _ = data_in_sender.send(BytesMut::from(&buf[..len])).await;
                    if fin {
                        _ = data_in_sender.send(BytesMut::new()).await;
                    }
                }
                Err(quiche::Error::Done) => {
                    break;
                }
                Err(e) => {
                    log::warn!("Error quic stream_readable: {e},recv_info={recv_info:?}");
                    break 'out;
                }
            }
        }
    }
    Ok(())
}
#[derive(Debug)]
enum Event {
    Output(BytesMut),
    Input(BytesMut),
    Timeout,
}
async fn all_event(
    input: &mut Receiver<BytesMut>,
    data_out_receiver: &mut Receiver<BytesMut>,
    sleep_until: &mut Pin<Box<Sleep>>,
) -> io::Result<Event> {
    tokio::select! {
        rs=input.recv()=>{
            let buf = rs.ok_or(Error::other("input close"))?;
            Ok(Event::Input(buf))
        }
        rs=data_out_receiver.recv()=>{
            let buf = rs.ok_or(Error::other("output close"))?;
            Ok(Event::Output(buf))
        }
        _=sleep_until=>{
            Ok(Event::Timeout)
        }
    }
}
async fn input_event(
    input: &mut Receiver<BytesMut>,
    sleep_until: &mut Pin<Box<Sleep>>,
) -> io::Result<Event> {
    tokio::select! {
        rs=input.recv()=>{
            let buf = rs.ok_or(Error::other("input close"))?;
            Ok(Event::Input(buf))
        }
        _=sleep_until=>{
            Ok(Event::Timeout)
        }
    }
}
async fn output_event(
    data_out_receiver: &mut Receiver<BytesMut>,
    sleep_until: &mut Pin<Box<Sleep>>,
) -> io::Result<Event> {
    tokio::select! {
        rs=data_out_receiver.recv()=>{
            let buf = rs.ok_or(Error::other("output close"))?;
            Ok(Event::Output(buf))
        }
        _=sleep_until=>{
            Ok(Event::Timeout)
        }
    }
}
