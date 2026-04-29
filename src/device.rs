use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use smoltcp::{
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
};
use tokio::sync::mpsc::{unbounded_channel, Permit, Sender, UnboundedReceiver, UnboundedSender};

use crate::packet::AnyIpPktFrame;

pub(super) struct VirtualDevice {
    in_buf_avail: Arc<AtomicBool>,
    in_buf: UnboundedReceiver<Vec<u8>>,
    out_buf: Sender<AnyIpPktFrame>,
    mtu: usize,
}

impl VirtualDevice {
    pub(super) fn new(
        iface_egress_tx: Sender<AnyIpPktFrame>,
        mtu: usize,
    ) -> (Self, UnboundedSender<Vec<u8>>, Arc<AtomicBool>) {
        let iface_ingress_tx_avail = Arc::new(AtomicBool::new(false));
        let (iface_ingress_tx, iface_ingress_rx) = unbounded_channel();
        (
            Self {
                in_buf_avail: iface_ingress_tx_avail.clone(),
                in_buf: iface_ingress_rx,
                out_buf: iface_egress_tx,
                mtu,
            },
            iface_ingress_tx,
            iface_ingress_tx_avail,
        )
    }
}

impl Device for VirtualDevice {
    type RxToken<'a> = VirtualRxToken;
    type TxToken<'a> = VirtualTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let Ok(buffer) = self.in_buf.try_recv() else {
            self.in_buf_avail.store(false, Ordering::Release);
            return None;
        };

        // When the outbound queue is full, still process ingress (including ACKs) so smoltcp can
        // update the window and drain its internal tx_buffer. Response frames for this cycle are
        // dropped via Null; the peer can recover via TCP retransmission.
        let tx = match self.out_buf.try_reserve() {
            Ok(permit) => VirtualTxToken::Normal(permit),
            Err(_) => VirtualTxToken::Null,
        };

        Some((Self::RxToken { buffer }, tx))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        match self.out_buf.try_reserve() {
            Ok(permit) => Some(VirtualTxToken::Normal(permit)),
            Err(_) => None,
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ip;
        capabilities.max_transmission_unit = self.mtu;
        capabilities
    }
}

pub(super) struct VirtualRxToken {
    buffer: Vec<u8>,
}

impl RxToken for VirtualRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer[..])
    }
}

/// When the egress channel has capacity, use `Normal` to deliver generated IP packets.
/// When full, use `Null`: smoltcp write callbacks still run but nothing is sent upstream.
pub(super) enum VirtualTxToken<'a> {
    Normal(Permit<'a, Vec<u8>>),
    Null,
}

impl<'a> TxToken for VirtualTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        if let Self::Normal(permit) = self {
            permit.send(buffer);
        }
        result
    }
}
