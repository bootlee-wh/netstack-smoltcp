use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll, Waker},
};

use futures::Stream;
use smoltcp::{
    iface::{Config as InterfaceConfig, Interface, SocketHandle, SocketSet},
    phy::Device,
    socket::tcp::{Socket as TcpSocket, SocketBuffer as TcpSocketBuffer, State as TcpState},
    storage::RingBuffer,
    time::{Duration, Instant},
    wire::{HardwareAddress, IpAddress, IpCidr, IpProtocol, Ipv4Address, Ipv6Address, TcpPacket},
};
use spin::Mutex as SpinMutex;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{
        mpsc::{unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender},
        Notify,
    },
};
use tracing::{error, trace};

use crate::{
    device::VirtualDevice,
    packet::{AnyIpPktFrame, IpPacket},
    Runner,
};

/// Timeout for a socket that has not finished the 3-way handshake (Listen /
/// SynReceived). Once established the socket switches to `ESTABLISHED_TIMEOUT`.
/// Without it a half-open socket (client gave up) keeps both window buffers for
/// up to two hours.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
// FIXME: It should follow system's setting. 7200 is Linux's default.
const ESTABLISHED_TIMEOUT: Duration = Duration::from_secs(7200);

static TCP_SOCKETS_ALIVE: AtomicUsize = AtomicUsize::new(0);
static TCP_SOCKETS_HANDSHAKING: AtomicUsize = AtomicUsize::new(0);
static TCP_DUP_SYN_IGNORED: AtomicU64 = AtomicU64::new(0);
static TCP_HANDSHAKE_ABORTED: AtomicU64 = AtomicU64::new(0);

/// Process-wide TCP socket counters (summed over all stacks), for memory diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpStats {
    /// smoltcp sockets currently alive (each holds the rx/tx window buffers).
    pub sockets: usize,
    /// Sockets still in Listen / SynReceived.
    pub handshaking: usize,
    /// Retransmitted SYNs routed to the existing handshaking socket instead of
    /// creating another one.
    pub dup_syn_ignored: u64,
    /// Sockets closed before the handshake completed (timeout / RST).
    pub handshake_aborted: u64,
}

pub fn tcp_stats() -> TcpStats {
    TcpStats {
        sockets: TCP_SOCKETS_ALIVE.load(Ordering::Relaxed),
        handshaking: TCP_SOCKETS_HANDSHAKING.load(Ordering::Relaxed),
        dup_syn_ignored: TCP_DUP_SYN_IGNORED.load(Ordering::Relaxed),
        handshake_aborted: TCP_HANDSHAKE_ABORTED.load(Ordering::Relaxed),
    }
}

type ConnKey = (SocketAddr, SocketAddr);
/// 4-tuples whose socket is still handshaking; shared by `handle_packet` (dedup
/// on SYN) and `handle_socket` (removal once the handshake ends).
type SharedPending = Arc<SpinMutex<HashSet<ConnKey>>>;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TcpSocketState {
    Normal,
    Close,
    Closing,
    Closed,
}

struct TcpSocketControl {
    send_buffer: RingBuffer<'static, u8>,
    send_waker: Option<Waker>,
    recv_buffer: RingBuffer<'static, u8>,
    recv_waker: Option<Waker>,
    recv_state: TcpSocketState,
    send_state: TcpSocketState,
    // The stream-side buffers start empty and are allocated on first use, so a
    // connection that never carries data in a direction does not pay for it.
    recv_buffer_size: usize,
    send_buffer_size: usize,
}

impl TcpSocketControl {
    fn ensure_recv_buffer(&mut self) {
        if self.recv_buffer.capacity() == 0 {
            self.recv_buffer = RingBuffer::new(vec![0u8; self.recv_buffer_size]);
        }
    }

    fn ensure_send_buffer(&mut self) {
        if self.send_buffer.capacity() == 0 {
            self.send_buffer = RingBuffer::new(vec![0u8; self.send_buffer_size]);
        }
    }
}

struct TcpSocketCreation {
    control: SharedControl,
    socket: TcpSocket<'static>,
    key: ConnKey,
}

struct TcpSocketEntry {
    control: SharedControl,
    key: ConnKey,
    handshaking: bool,
}

type SharedNotify = Arc<Notify>;
type SharedControl = Arc<SpinMutex<TcpSocketControl>>;

struct TcpListenerRunner;

impl TcpListenerRunner {
    #[allow(clippy::too_many_arguments)]
    fn create(
        device: VirtualDevice,
        iface: Interface,
        iface_ingress_tx: UnboundedSender<Vec<u8>>,
        iface_ingress_tx_avail: Arc<AtomicBool>,
        tcp_rx: Receiver<AnyIpPktFrame>,
        stream_tx: UnboundedSender<TcpStream>,
        sockets: HashMap<SocketHandle, TcpSocketEntry>,
        tcp_recv_buffer_size: u32,
        tcp_send_buffer_size: u32,
    ) -> Runner {
        Runner::new(async move {
            let notify = Arc::new(Notify::new());
            let (socket_tx, socket_rx) = unbounded_channel::<TcpSocketCreation>();
            let pending: SharedPending = Arc::new(SpinMutex::new(HashSet::new()));
            let res = tokio::select! {
                v = Self::handle_packet(notify.clone(), iface_ingress_tx, iface_ingress_tx_avail.clone(), tcp_rx, stream_tx, socket_tx, pending.clone(), tcp_recv_buffer_size, tcp_send_buffer_size) => v,
                v = Self::handle_socket(notify, device, iface, iface_ingress_tx_avail, sockets, socket_rx, pending) => v,
            };
            res?;
            trace!("VirtDevice::poll thread exited");
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_packet(
        notify: SharedNotify,
        iface_ingress_tx: UnboundedSender<Vec<u8>>,
        iface_ingress_tx_avail: Arc<AtomicBool>,
        mut tcp_rx: Receiver<AnyIpPktFrame>,
        stream_tx: UnboundedSender<TcpStream>,
        socket_tx: UnboundedSender<TcpSocketCreation>,
        pending: SharedPending,
        tcp_recv_buffer_size: u32,
        tcp_send_buffer_size: u32,
    ) -> std::io::Result<()> {
        while let Some(frame) = tcp_rx.recv().await {
            let packet = match IpPacket::new_checked(frame.as_slice()) {
                Ok(p) => p,
                Err(err) => {
                    error!("invalid TCP IP packet: {:?}", err,);
                    continue;
                }
            };

            // Specially handle icmp packet by TCP interface.
            if matches!(packet.protocol(), IpProtocol::Icmp | IpProtocol::Icmpv6) {
                iface_ingress_tx
                    .send(frame)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
                iface_ingress_tx_avail.store(true, Ordering::Release);
                notify.notify_one();
                continue;
            }

            let src_ip = packet.src_addr();
            let dst_ip = packet.dst_addr();
            let payload = packet.payload();

            let packet = match TcpPacket::new_checked(payload) {
                Ok(p) => p,
                Err(err) => {
                    error!("invalid TCP err: {err}, src_ip: {src_ip}, dst_ip: {dst_ip}, payload: {payload:?}");
                    continue;
                }
            };
            let src_port = packet.src_port();
            let dst_port = packet.dst_port();

            let src_addr = SocketAddr::new(src_ip, src_port);
            let dst_addr = SocketAddr::new(dst_ip, dst_port);

            // TCP first handshake packet, create a new Connection
            if packet.syn() && !packet.ack() {
                let key = (src_addr, dst_addr);
                // A retransmitted SYN (client got no SYN-ACK in time, e.g. while
                // the host is stalled) must reach the socket that is already
                // handshaking; creating another one per retransmit multiplies the
                // window buffers and hands duplicate streams to the caller.
                if !pending.lock().insert(key) {
                    TCP_DUP_SYN_IGNORED.fetch_add(1, Ordering::Relaxed);
                    trace!("retransmitted SYN for {} <-> {}", src_addr, dst_addr);
                    iface_ingress_tx
                        .send(frame)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
                    iface_ingress_tx_avail.store(true, Ordering::Release);
                    notify.notify_one();
                    continue;
                }

                let mut socket = TcpSocket::new(
                    TcpSocketBuffer::new(vec![0u8; tcp_recv_buffer_size as usize]),
                    TcpSocketBuffer::new(vec![0u8; tcp_send_buffer_size as usize]),
                );
                socket.set_keep_alive(Some(Duration::from_secs(28)));
                socket.set_timeout(Some(HANDSHAKE_TIMEOUT));
                // NO ACK delay
                // socket.set_ack_delay(None);

                if let Err(err) = socket.listen(dst_addr) {
                    error!("listen error: {:?}", err);
                    pending.lock().remove(&key);
                    continue;
                }

                trace!("created TCP connection for {} <-> {}", src_addr, dst_addr);

                let control = Arc::new(SpinMutex::new(TcpSocketControl {
                    send_buffer: RingBuffer::new(Vec::new()),
                    send_waker: None,
                    recv_buffer: RingBuffer::new(Vec::new()),
                    recv_waker: None,
                    recv_state: TcpSocketState::Normal,
                    send_state: TcpSocketState::Normal,
                    recv_buffer_size: tcp_recv_buffer_size as usize,
                    send_buffer_size: tcp_send_buffer_size as usize,
                }));
                TCP_SOCKETS_ALIVE.fetch_add(1, Ordering::Relaxed);
                TCP_SOCKETS_HANDSHAKING.fetch_add(1, Ordering::Relaxed);

                stream_tx
                    .send(TcpStream {
                        src_addr,
                        dst_addr,
                        notify: notify.clone(),
                        control: control.clone(),
                    })
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
                socket_tx
                    .send(TcpSocketCreation {
                        control,
                        socket,
                        key,
                    })
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
            }

            // Pipeline tcp stream packet
            iface_ingress_tx
                .send(frame)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
            iface_ingress_tx_avail.store(true, Ordering::Release);
            notify.notify_one();
        }
        Ok(())
    }

    async fn handle_socket(
        notify: SharedNotify,
        mut device: VirtualDevice,
        mut iface: Interface,
        iface_ingress_tx_avail: Arc<AtomicBool>,
        mut sockets: HashMap<SocketHandle, TcpSocketEntry>,
        mut socket_rx: UnboundedReceiver<TcpSocketCreation>,
        pending: SharedPending,
    ) -> std::io::Result<()> {
        let mut socket_set = SocketSet::new(vec![]);
        loop {
            while let Ok(TcpSocketCreation {
                control,
                socket,
                key,
            }) = socket_rx.try_recv()
            {
                let handle = socket_set.add(socket);
                sockets.insert(
                    handle,
                    TcpSocketEntry {
                        control,
                        key,
                        handshaking: true,
                    },
                );
            }

            let before_poll = Instant::now();
            let updated_sockets = iface.poll(before_poll, &mut device, &mut socket_set);
            if matches!(
                updated_sockets,
                smoltcp::iface::PollResult::SocketStateChanged
            ) {
                trace!("VirtDevice::poll costed {}", Instant::now() - before_poll);
            }

            // Check all the sockets' status
            let mut sockets_to_remove = Vec::new();

            for (socket_handle, entry) in sockets.iter_mut() {
                let socket_handle = *socket_handle;
                let socket = socket_set.get_mut::<TcpSocket>(socket_handle);
                let mut control = entry.control.lock();

                if entry.handshaking
                    && !matches!(socket.state(), TcpState::Listen | TcpState::SynReceived)
                {
                    entry.handshaking = false;
                    pending.lock().remove(&entry.key);
                    TCP_SOCKETS_HANDSHAKING.fetch_sub(1, Ordering::Relaxed);
                    if socket.state() == TcpState::Closed {
                        TCP_HANDSHAKE_ABORTED.fetch_add(1, Ordering::Relaxed);
                    } else {
                        socket.set_timeout(Some(ESTABLISHED_TIMEOUT));
                    }
                }

                // Remove the socket only when it is in the closed state.
                if socket.state() == TcpState::Closed {
                    sockets_to_remove.push(socket_handle);

                    control.send_state = TcpSocketState::Closed;
                    control.recv_state = TcpSocketState::Closed;

                    if let Some(waker) = control.send_waker.take() {
                        waker.wake();
                    }
                    if let Some(waker) = control.recv_waker.take() {
                        waker.wake();
                    }

                    trace!("closed TCP connection");
                    continue;
                }

                // SHUT_WR — only close once the send_buffer has been fully
                // drained into the smoltcp socket.  Closing earlier transitions
                // the socket to FIN_WAIT_1, making can_send() return false, so
                // the send loop below never runs and the remaining data is lost.
                if matches!(control.send_state, TcpSocketState::Close)
                    && control.send_buffer.is_empty()
                {
                    trace!("closing TCP Write Half, {:?}", socket.state());

                    socket.close();
                    control.send_state = TcpSocketState::Closing;
                }

                // Check if readable
                let mut wake_receiver = false;
                if socket.can_recv() {
                    control.ensure_recv_buffer();
                }
                while socket.can_recv() && !control.recv_buffer.is_full() {
                    let result = socket.recv(|buffer| {
                        let n = control.recv_buffer.enqueue_slice(buffer);
                        (n, ())
                    });

                    match result {
                        Ok(..) => wake_receiver = true,
                        Err(err) => {
                            error!("socket recv error: {:?}, {:?}", err, socket.state());

                            // Don't know why. Abort the connection.
                            socket.abort();

                            if matches!(control.recv_state, TcpSocketState::Normal) {
                                control.recv_state = TcpSocketState::Closed;
                            }
                            wake_receiver = true;

                            // The socket will be recycled in the next poll.
                            break;
                        }
                    }
                }

                // If socket is not in ESTABLISH, FIN-WAIT-1, FIN-WAIT-2,
                // the local client have closed our receiver.
                let states = [
                    TcpState::Listen,
                    TcpState::SynReceived,
                    TcpState::Established,
                    TcpState::FinWait1,
                    TcpState::FinWait2,
                ];
                if matches!(control.recv_state, TcpSocketState::Normal)
                    && !socket.may_recv()
                    && !states.contains(&socket.state())
                {
                    trace!("closed TCP Read Half, {:?}", socket.state());

                    // Let TcpStream::poll_read returns EOF.
                    control.recv_state = TcpSocketState::Closed;
                    wake_receiver = true;
                }

                if wake_receiver && control.recv_waker.is_some() {
                    if let Some(waker) = control.recv_waker.take() {
                        waker.wake();
                    }
                }

                // Check if writable
                let mut wake_sender = false;
                while socket.can_send() && !control.send_buffer.is_empty() {
                    let result = socket.send(|buffer| {
                        let n = control.send_buffer.dequeue_slice(buffer);
                        (n, ())
                    });

                    match result {
                        Ok(..) => wake_sender = true,
                        Err(err) => {
                            error!("socket send error: {:?}, {:?}", err, socket.state());

                            // Don't know why. Abort the connection.
                            socket.abort();

                            if matches!(control.send_state, TcpSocketState::Normal) {
                                control.send_state = TcpSocketState::Closed;
                            }
                            wake_sender = true;

                            // The socket will be recycled in the next poll.
                            break;
                        }
                    }
                }

                if wake_sender && control.send_waker.is_some() {
                    if let Some(waker) = control.send_waker.take() {
                        waker.wake();
                    }
                }
            }

            for socket_handle in sockets_to_remove {
                sockets.remove(&socket_handle);
                socket_set.remove(socket_handle);
                TCP_SOCKETS_ALIVE.fetch_sub(1, Ordering::Relaxed);
            }

            // Ensure every loop iteration yields to the runtime. When egress
            // capacity is available OR when `poll_delay` returns ZERO ("re-poll
            // now"), this loop otherwise takes no `.await`, so under an active
            // flow it busy-spins. Because `handle_socket` and `handle_packet`
            // are arms of one `tokio::select!` future, a never-yielding
            // `handle_socket` never returns control to the sibling SYN-accept
            // arm: one core pins flat at 100% and new inbound connections are
            // starved (observed as connect timeouts with no SYN-ACK). A
            // cooperative `yield_now()` keeps the re-poll just as prompt while
            // letting `handle_packet` make progress. The timed-park path and the
            // 5 ms idle fallback are unchanged, so there is no missed-wake risk.
            if iface_ingress_tx_avail.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            } else {
                let next_duration = iface
                    .poll_delay(before_poll, &socket_set)
                    .unwrap_or(Duration::from_millis(5));
                if next_duration != Duration::ZERO {
                    let _ = tokio::time::timeout(
                        tokio::time::Duration::from(next_duration),
                        notify.notified(),
                    )
                    .await;
                } else {
                    tokio::task::yield_now().await;
                }
            }
        }
    }
}

pub struct TcpListener {
    stream_rx: UnboundedReceiver<TcpStream>,
}

impl TcpListener {
    pub(super) fn new(
        tcp_rx: Receiver<AnyIpPktFrame>,
        stack_tx: Sender<AnyIpPktFrame>,
        mtu: usize,
        tcp_recv_buffer_size: u32,
        tcp_send_buffer_size: u32,
    ) -> std::io::Result<(Runner, Self)> {
        let (mut device, iface_ingress_tx, iface_ingress_tx_avail) =
            VirtualDevice::new(stack_tx, mtu);
        let iface = Self::create_interface(&mut device)?;

        let (stream_tx, stream_rx) = unbounded_channel();

        let runner = TcpListenerRunner::create(
            device,
            iface,
            iface_ingress_tx,
            iface_ingress_tx_avail,
            tcp_rx,
            stream_tx,
            HashMap::new(),
            tcp_recv_buffer_size,
            tcp_send_buffer_size,
        );

        Ok((runner, Self { stream_rx }))
    }

    fn create_interface<D>(device: &mut D) -> std::io::Result<Interface>
    where
        D: Device + ?Sized,
    {
        let mut iface_config = InterfaceConfig::new(HardwareAddress::Ip);
        iface_config.random_seed = rand::random();
        let mut iface = Interface::new(iface_config, device, Instant::now());
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(IpAddress::v4(0, 0, 0, 1), 0))
                .expect("iface IPv4");
            ip_addrs
                .push(IpCidr::new(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1), 0))
                .expect("iface IPv6");
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(0, 0, 0, 1))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, e))?;
        iface
            .routes_mut()
            .add_default_ipv6_route(Ipv6Address::new(0, 0, 0, 0, 0, 0, 0, 1))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, e))?;
        iface.set_any_ip(true);
        Ok(iface)
    }
}

impl Stream for TcpListener {
    type Item = (TcpStream, SocketAddr, SocketAddr);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.stream_rx.poll_recv(cx).map(|stream| {
            stream.map(|stream| {
                let local_addr = *stream.local_addr();
                let remote_addr: SocketAddr = *stream.remote_addr();
                (stream, local_addr, remote_addr)
            })
        })
    }
}

pub struct TcpStream {
    src_addr: SocketAddr,
    dst_addr: SocketAddr,
    notify: SharedNotify,
    control: SharedControl,
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let mut control = self.control.lock();

        if matches!(control.recv_state, TcpSocketState::Normal) {
            control.recv_state = TcpSocketState::Close;
        }

        if matches!(control.send_state, TcpSocketState::Normal) {
            control.send_state = TcpSocketState::Close;
        }

        self.notify.notify_one();
    }
}

impl TcpStream {
    pub fn local_addr(&self) -> &SocketAddr {
        &self.src_addr
    }

    pub fn remote_addr(&self) -> &SocketAddr {
        &self.dst_addr
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut control = self.control.lock();

        // Read from buffer
        if control.recv_buffer.is_empty() {
            // If socket is already closed / half closed, just return EOF directly.
            if matches!(control.recv_state, TcpSocketState::Closed) {
                return Ok(()).into();
            }

            // Nothing could be read. Wait for notify.
            if let Some(old_waker) = control.recv_waker.replace(cx.waker().clone()) {
                if !old_waker.will_wake(cx.waker()) {
                    old_waker.wake();
                }
            }

            return Poll::Pending;
        }

        let recv_buf = buf.initialize_unfilled();
        let n = control.recv_buffer.dequeue_slice(recv_buf);
        buf.advance(n);

        if n > 0 {
            self.notify.notify_one();
        }

        Ok(()).into()
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut control = self.control.lock();

        // If state == Close | Closing | Closed, the TCP stream WR half is closed.
        if !matches!(control.send_state, TcpSocketState::Normal) {
            return Err(std::io::ErrorKind::BrokenPipe.into()).into();
        }

        // Write to buffer
        control.ensure_send_buffer();

        if control.send_buffer.is_full() {
            if let Some(old_waker) = control.send_waker.replace(cx.waker().clone()) {
                if !old_waker.will_wake(cx.waker()) {
                    old_waker.wake();
                }
            }

            return Poll::Pending;
        }

        let n = control.send_buffer.enqueue_slice(buf);

        if n > 0 {
            self.notify.notify_one();
        }

        Ok(n).into()
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Ok(()).into()
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut control = self.control.lock();

        if matches!(control.send_state, TcpSocketState::Closed) {
            return Ok(()).into();
        }

        // SHUT_WR
        if matches!(control.send_state, TcpSocketState::Normal) {
            control.send_state = TcpSocketState::Close;
        }

        if let Some(old_waker) = control.send_waker.replace(cx.waker().clone()) {
            if !old_waker.will_wake(cx.waker()) {
                old_waker.wake();
            }
        }

        self.notify.notify_one();

        Poll::Pending
    }
}
