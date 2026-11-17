//! Text command protocol on the CAN/control serial port.
//!
//! Classic CAN and CAN FD share one text grammar. Commands are newline-terminated;
//! the line reader tolerates a command split across several USB packets and several
//! commands arriving in one packet (frame boundaries are the `\n` bytes).
//!
//! Host -> device:
//! * `PING`
//! * `SWD ID`
//! * `CAN TX <id:3hex> <data:hex|->`                (classic frame, 0-8 bytes)
//! * `CANFD TX <id:3hex> <len> <F|B> <data:hex|->`  (CAN FD frame, B = BRS)
//! * `CAN RX` / `CANFD RX`                          (blocking read, 100 ms timeout)
//!
//! Device -> host (unsolicited, for the bidirectional bridge):
//! * `RX <id:3hex> <len> <data:hex|->`               (classic frame received)
//! * `RXFD <id:3hex> <len> <F|B> <data:hex|->`       (CAN FD frame received)
//!
//! Responses to commands carry the same shape prefixed with `OK `.

use core::fmt::Write as _;

use embassy_stm32::can::frame::{FdEnvelope, FdFrame, Header};
use embassy_stm32::can::{BufferedFdCanReceiver, BufferedFdCanSender};
use embassy_time::{Duration, with_timeout};
use embedded_can::Id;
use heapless::String;

use crate::swd::Swd;

/// Fits the longest frame line: 64-byte payload as 128 hex chars plus prefix.
type Reply = String<255>;

/// Largest possible command line (a 64-byte FD payload is 128 hex chars).
const LINE_CAPACITY: usize = 200;

pub struct Line {
    bytes: [u8; LINE_CAPACITY],
    len: usize,
}

impl Line {
    pub const fn new() -> Self {
        Self {
            bytes: [0; LINE_CAPACITY],
            len: 0,
        }
    }

    pub fn push(&mut self, byte: u8) -> Option<&str> {
        if byte == b'\n' {
            let n = core::mem::take(&mut self.len);
            // A full buffer may contain a truncated command. Discard that line.
            if n == self.bytes.len() {
                return Some("");
            }
            return Some(core::str::from_utf8(&self.bytes[..n]).unwrap_or_default());
        }
        if self.len < self.bytes.len() {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
        None
    }
}

fn fixed(text: &str) -> Reply {
    Reply::try_from(text).expect("fixed reply fits")
}

/// Handle one newline-terminated command.
pub async fn execute(
    command: &str,
    can_tx: &mut BufferedFdCanSender,
    can_rx: &BufferedFdCanReceiver,
    swd: &mut Option<Swd>,
) -> Reply {
    let mut words = command.split_ascii_whitespace();
    match (words.next(), words.next()) {
        (Some("PING"), None) => fixed("OK DM-MC02\n"),
        (Some("SWD"), Some("ID")) if words.next().is_none() => {
            let Some(swd) = swd.as_mut() else {
                return fixed("ERR SWD_NOT_CONFIGURED\n");
            };
            match swd.read_dpidr() {
                Some(id) => {
                    let mut out = Reply::new();
                    let _ = writeln!(out, "OK DPIDR {id:08X}");
                    out
                }
                None => fixed("ERR SWD\n"),
            }
        }
        (Some("CAN"), Some("TX")) => {
            let (Some(id), Some(hex), None) = (words.next(), words.next(), words.next()) else {
                return fixed("ERR SYNTAX\n");
            };
            let Some(frame) = parse_classic_frame(id, hex) else {
                return fixed("ERR FRAME\n");
            };
            match with_timeout(Duration::from_millis(100), can_tx.write(frame)).await {
                Ok(()) => fixed("OK QUEUED\n"),
                Err(_) => fixed("ERR CAN TX\n"),
            }
        }
        (Some("CANFD"), Some("TX")) => {
            let (Some(id), Some(len), Some(flags), Some(hex), None) =
                (words.next(), words.next(), words.next(), words.next(), words.next())
            else {
                return fixed("ERR SYNTAX\n");
            };
            let Some(frame) = parse_fd_frame(id, len, flags, hex) else {
                return fixed("ERR FRAME\n");
            };
            match with_timeout(Duration::from_millis(100), can_tx.write(frame)).await {
                Ok(()) => fixed("OK QUEUED\n"),
                Err(_) => fixed("ERR CAN TX\n"),
            }
        }
        (Some("CAN"), Some("RX")) if words.next().is_none() => recv_reply(can_rx).await,
        (Some("CANFD"), Some("RX")) if words.next().is_none() => recv_reply(can_rx).await,
        _ => fixed("ERR SYNTAX\n"),
    }
}

/// Blocking read with a short timeout, formatted the same way as the unsolicited
/// forwarding lines but prefixed with `OK `.
async fn recv_reply(can_rx: &BufferedFdCanReceiver) -> Reply {
    match with_timeout(Duration::from_millis(100), can_rx.receive()).await {
        Ok(Ok(env)) => {
            let mut out = Reply::new();
            let _ = out.push_str("OK ");
            let _ = write_frame(&mut out, &env);
            out.push('\n').expect("line ending fits");
            out
        }
        Ok(Err(_)) => fixed("ERR CAN RX\n"),
        Err(_) => fixed("EMPTY\n"),
    }
}

/// Format a received frame as an unsolicited forwarding line (no `OK` prefix).
pub fn format_frame(env: &FdEnvelope) -> Reply {
    let mut out = Reply::new();
    let _ = write_frame(&mut out, env);
    out.push('\n').expect("line ending fits");
    out
}

fn write_frame(out: &mut Reply, env: &FdEnvelope) -> Result<(), ()> {
    let frame = &env.frame;
    let header = frame.header();
    let Id::Standard(id) = *header.id() else {
        return out.push_str("ERR UNSUPPORTED_FRAME").map_err(|_| ());
    };
    if header.rtr() {
        return out.push_str("ERR UNSUPPORTED_FRAME").map_err(|_| ());
    }
    let id = id.as_raw();
    if header.fdcan() {
        let flags = if header.bit_rate_switching() { "B" } else { "F" };
        let _ = write!(out, "RXFD {id:03X} {} {flags} ", header.len());
    } else {
        let _ = write!(out, "RX {id:03X} {} ", header.len());
    }
    let data = frame.data();
    if data.is_empty() {
        let _ = out.push('-');
    } else {
        for byte in data {
            let _ = write!(out, "{byte:02X}");
        }
    }
    Ok(())
}

/// Parse a classic CAN frame: `<3-hex-id> <hex|->`, 0-8 bytes.
fn parse_classic_frame(id: &str, payload: &str) -> Option<FdFrame> {
    let id = u16::from_str_radix(id, 16).ok()?;
    let (bytes, len) = parse_hex(payload, 8)?;
    // Classic frame: FdFrame with a non-FD header is transmitted as a classic frame.
    FdFrame::new_standard(id, &bytes[..len]).ok()
}

/// Parse a CAN FD frame: `<3-hex-id> <len> <F|B> <hex|->`, up to 64 bytes.
fn parse_fd_frame(id: &str, len: &str, flags: &str, payload: &str) -> Option<FdFrame> {
    let id = u16::from_str_radix(id, 16).ok()?;
    let len: usize = len.parse().ok()?;
    if !is_fd_len(len) {
        return None;
    }
    let brs = match flags {
        "F" => false,
        "B" => true,
        _ => return None,
    };
    let (bytes, n) = parse_hex(payload, 64)?;
    if n != len {
        return None;
    }
    let header = Header::new_fd(Id::Standard(embedded_can::StandardId::new(id)?), len as u8, false, brs);
    FdFrame::new(header, &bytes).ok()
}

fn parse_hex(payload: &str, max: usize) -> Option<(heapless::Vec<u8, 64>, usize)> {
    let payload = if payload == "-" { "" } else { payload };
    if !payload.is_ascii() || payload.len() > max * 2 || !payload.len().is_multiple_of(2) {
        return None;
    }
    let n = payload.len() / 2;
    let mut bytes = heapless::Vec::<u8, 64>::new();
    for i in 0..n {
        bytes.push(u8::from_str_radix(&payload[i * 2..i * 2 + 2], 16).ok()?).ok()?;
    }
    Some((bytes, n))
}

fn is_fd_len(len: usize) -> bool {
    matches!(len, 0..=8 | 12 | 16 | 20 | 24 | 32 | 48 | 64)
}
