use std::time::Duration;

use futures::{SinkExt, StreamExt};
use netstack_smoltcp::{tcp::tcp_stats, StackBuilder};

fn syn_frame() -> Vec<u8> {
    let builder = etherparse::PacketBuilder::ipv4([10, 0, 0, 2], [1, 2, 3, 4], 64)
        .tcp(40000, 443, 1000, 65535)
        .syn();
    let mut frame = Vec::with_capacity(builder.size(0));
    builder.write(&mut frame, &[]).unwrap();
    frame
}

/// A retransmitted SYN for a 4-tuple that is still handshaking must be routed
/// to the existing socket: one socket, one stream, a SYN-ACK still answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retransmitted_syn_reuses_handshaking_socket() {
    let (mut stack, runner, _udp, listener) =
        StackBuilder::default().enable_tcp(true).build().unwrap();
    tokio::spawn(runner.unwrap());
    let mut listener = listener.unwrap();

    stack.send(syn_frame()).await.unwrap();
    stack.send(syn_frame()).await.unwrap();

    // The stack still answers with a SYN-ACK.
    let reply = tokio::time::timeout(Duration::from_secs(2), stack.next())
        .await
        .expect("no SYN-ACK")
        .unwrap()
        .unwrap();
    let ip = etherparse::SlicedPacket::from_ip(&reply).unwrap();
    match ip.transport {
        Some(etherparse::TransportSlice::Tcp(tcp)) => assert!(tcp.syn() && tcp.ack()),
        other => panic!("unexpected reply: {other:?}"),
    }

    // Exactly one stream is handed out.
    let first = tokio::time::timeout(Duration::from_secs(2), listener.next()).await;
    assert!(matches!(first, Ok(Some(_))), "first stream missing");
    let second = tokio::time::timeout(Duration::from_millis(300), listener.next()).await;
    assert!(
        second.is_err(),
        "duplicate stream created for retransmitted SYN"
    );

    let stats = tcp_stats();
    assert_eq!(stats.sockets, 1);
    assert_eq!(stats.handshaking, 1);
    assert_eq!(stats.dup_syn_ignored, 1);
}
