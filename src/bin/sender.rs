//! Encodes a deterministic payload into Constellation pshreds and sends each
//! one as an individual UDP datagram to `receiver_addr`. Used by the
//! docker/tc-netem loss harness; run via `sender <host:port> [payload_bytes]`.

use std::env;
use std::net::UdpSocket;

use constellation_encoder::ConstellationEncoder;

fn deterministic_payload(len: usize) -> Vec<u8> {
    // Fixed pattern (not random) so the receiver can independently recompute
    // the expected payload without any out-of-band coordination.
    (0..len).map(|i| (i % 256) as u8).collect()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let receiver_addr = args.get(1).expect("usage: sender <host:port> [payload_bytes]");
    let payload_len: usize = args
        .get(2)
        .map(|s| s.parse().expect("payload_bytes must be a number"))
        .unwrap_or(65536);

    let payload = deterministic_payload(payload_len);
    let encoder = ConstellationEncoder::new();
    let shreds = encoder.encode(&payload).expect("encode should succeed");

    let socket = UdpSocket::bind("0.0.0.0:0").expect("failed to bind sender socket");
    socket
        .connect(receiver_addr)
        .expect("failed to connect to receiver");

    println!(
        "sender: encoded {} bytes into {} pshreds, sending to {receiver_addr}",
        payload_len,
        shreds.len()
    );

    // Each datagram is framed as a 4-byte BE shard-index prefix followed by
    // the shard bytes. The index prefix is needed at this layer because only
    // *data* shards carry a readable index in their own header (written
    // before RS encoding) -- parity shards are pure RS-encoded output with no
    // self-describing index, so the receiver can't recover shard position
    // from content alone for those.
    //
    // Single pass, no retransmission: pshreds are sent once per attester in
    // Constellation (not retried by the proposer), and retransmitting here
    // would give each shred multiple independent chances to survive netem
    // loss, masking the true single-shot 64-of-256 recovery threshold this
    // harness is meant to probe.
    for (i, shred) in shreds.iter().enumerate() {
        let mut packet = Vec::with_capacity(4 + shred.len());
        packet.extend_from_slice(&(i as u32).to_be_bytes());
        packet.extend_from_slice(shred);
        let _ = socket.send(&packet);
    }

    println!("sender: done");
}
