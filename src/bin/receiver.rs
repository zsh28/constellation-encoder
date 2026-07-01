//! Listens for UDP pshreds, waits a fixed collection window, decodes, and
//! verifies against the same deterministic payload the sender generates.
//! Exits 0 on successful recovery + match, 1 otherwise. Used by the
//! docker/tc-netem loss harness; run via
//! `receiver <bind_host:port> [payload_bytes]`.

use std::env;
use std::net::UdpSocket;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use constellation_encoder::{ConstellationEncoder, TOTAL_PSHREDS};

fn deterministic_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 256) as u8).collect()
}

const LISTEN_WINDOW: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let bind_addr = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "0.0.0.0:9000".to_string());
    let payload_len: usize = args
        .get(2)
        .map(|s| s.parse().expect("payload_bytes must be a number"))
        .unwrap_or(65536);

    let expected = deterministic_payload(payload_len);

    let socket = UdpSocket::bind(&bind_addr).expect("failed to bind receiver socket");
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("failed to set read timeout");

    let mut shreds: Vec<Option<Vec<u8>>> = vec![None; TOTAL_PSHREDS];
    let mut buf = vec![0u8; 65536];
    let deadline = Instant::now() + LISTEN_WINDOW;

    while Instant::now() < deadline {
        match socket.recv(&mut buf) {
            Ok(n) if n >= 4 => {
                // First 4 bytes are the UDP-framing shard-index prefix (see
                // sender.rs), not part of the shard content itself -- parity
                // shards have no self-describing index after RS encoding.
                let index = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
                if index < TOTAL_PSHREDS {
                    shreds[index] = Some(buf[4..n].to_vec());
                }
            }
            _ => {}
        }
    }

    let present = shreds.iter().filter(|s| s.is_some()).count();
    println!("receiver: collected {present}/{TOTAL_PSHREDS} pshreds within {LISTEN_WINDOW:?}");

    let encoder = ConstellationEncoder::new();
    match encoder.decode(&shreds) {
        Ok(recovered) if recovered == expected => {
            println!("receiver: PASS (recovered {} bytes correctly)", recovered.len());
            ExitCode::SUCCESS
        }
        Ok(_) => {
            println!("receiver: FAIL (decoded but payload mismatch)");
            ExitCode::FAILURE
        }
        Err(e) => {
            println!("receiver: FAIL (decode error: {e})");
            ExitCode::FAILURE
        }
    }
}
