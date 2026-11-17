//! SocketCAN device endpoint speaking the Linux `gs_usb` protocol.
//!
//! This makes the DM-MC02 enumerate as a vendor-class USB device with one bulk IN
//! and one bulk OUT endpoint, exactly like the candleLight/CANtact adapters that the
//! mainline `gs_usb` kernel driver binds to. The Linux `can` framework then exposes the
//! board as a SocketCAN interface (e.g. `can0`), usable with `cansend` / `candump`.
//!
//! Base scope implemented here is Classical CAN standard/extended data frames. The
//! FDCAN keeps its fixed 1 Mbit/s nominal timing; the host must bring the interface up
//! with `bitrate 1000000` (see the README).

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either, select};
use embassy_stm32::can::frame::{FdEnvelope, FdFrame};
use embassy_stm32::can::{BufferedFdCanReceiver, BufferedFdCanSender};
use embassy_time::{Duration, with_timeout};
use embassy_usb::{
    Builder, Handler,
    control::{InResponse, OutResponse, Recipient, Request, RequestType},
    driver::{Driver, Endpoint, EndpointIn, EndpointOut},
    types::InterfaceNumber,
};
use embedded_can::Id;
use static_cell::StaticCell;

use crate::board::Usb;

/// Full-speed bulk endpoint max packet size.
const GS_PACKET: usize = 64;

// gs_usb vendor control requests (bRequest values).
const BREQ_HOST_FORMAT: u8 = 0;
const BREQ_BITTIMING: u8 = 1;
const BREQ_MODE: u8 = 2;
const BREQ_BT_CONST: u8 = 4;
const BREQ_DEVICE_CONFIG: u8 = 5;
const BREQ_DATA_BITTIMING: u8 = 11;
const BREQ_GET_STATE: u8 = 15;

// gs_can_mode values.
const MODE_RESET: u32 = 0;
const MODE_START: u32 = 1;

// linux/can.h flags packed into gs_host_frame::can_id.
const CAN_EFF_FLAG: u32 = 0x8000_0000;
const CAN_RTR_FLAG: u32 = 0x4000_0000;

// gs_host_frame::echo_id that marks a regular RX frame.
const ECHO_ID_RX: u32 = 0xFFFF_FFFF;

/// Set to `true` by `GS_USB_BREQ_MODE` with `GS_CAN_MODE_START` and cleared on reset.
static STARTED: AtomicBool = AtomicBool::new(false);

pub struct GsUsb {
    ep_out: <Usb as Driver<'static>>::EndpointOut,
    ep_in: <Usb as Driver<'static>>::EndpointIn,
}

struct GsHandler {
    iface: InterfaceNumber,
}

impl GsUsb {
    pub fn new(builder: &mut Builder<'static, Usb>) -> Self {
        static HANDLER: StaticCell<GsHandler> = StaticCell::new();
        let (ep_out, ep_in, iface) = {
            let mut function = builder.function(0xff, 0, 0);
            let mut interface = function.interface();
            let iface = interface.interface_number();
            let mut alt = interface.alt_setting(0xff, 0, 0, None);
            let ep_out = alt.endpoint_bulk_out(None, GS_PACKET as u16);
            let ep_in = alt.endpoint_bulk_in(None, GS_PACKET as u16);
            (ep_out, ep_in, iface)
        };
        builder.handler(HANDLER.init(GsHandler { iface }));
        Self { ep_out, ep_in }
    }

    pub async fn run(mut self, mut can_tx: BufferedFdCanSender, can_rx: BufferedFdCanReceiver) {
        let mut rx_buf = [0u8; GS_PACKET];
        loop {
            self.ep_out.wait_enabled().await;
            self.ep_in.wait_enabled().await;
            loop {
                // Before the Linux side brings `can0` up, drain only the OUT endpoint and
                // leave CAN RX frames alone so the serial bridge can own them.
                if !STARTED.load(Ordering::Relaxed) {
                    if self.ep_out.read(&mut rx_buf).await.is_err() {
                        break;
                    }
                    continue;
                }

                let host = self.ep_out.read(&mut rx_buf);
                let can = can_rx.receive();
                match select(host, can).await {
                    Either::First(Ok(n)) => {
                        if let Some(frame) = parse_host_frame(&rx_buf[..n])
                            && with_timeout(Duration::from_millis(100), can_tx.write(frame))
                                .await
                                .is_ok()
                        {
                            // Echo the frame back with its original echo_id so the host
                            // frees its TX context (candleLight-style echo).
                            let _ = self.ep_in.write(&rx_buf[..n]).await;
                        }
                    }
                    Either::First(Err(_)) => break,
                    Either::Second(Ok(env)) => {
                        if let Some(pkt) = build_rx_frame(&env) {
                            let _ = self.ep_in.write(&pkt).await;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

impl Handler for GsHandler {
    fn control_in<'a>(&'a mut self, req: Request, buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        if (req.request_type, req.recipient) != (RequestType::Vendor, Recipient::Interface) {
            return None;
        }
        match req.request {
            BREQ_DEVICE_CONFIG => {
                // 1 interface (icount = 0 -> channel count 1), firmware version 2.
                let resp: [u8; 12] = [0, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0];
                buf[..resp.len()].copy_from_slice(&resp);
                Some(InResponse::Accepted(&buf[..resp.len()]))
            }
            BREQ_BT_CONST => {
                // Classic CAN only, 80 MHz FDCAN clock. Values are the hardware limits
                // reported to the Linux `can` bittiming calculation.
                let mut resp = [0u8; 40];
                put_u32(&mut resp, 0, 0); // feature: no FD/one-shot/listen-only advertised
                put_u32(&mut resp, 4, 80_000_000); // fclk_can
                put_u32(&mut resp, 8, 1); // tseg1_min
                put_u32(&mut resp, 12, 256); // tseg1_max
                put_u32(&mut resp, 16, 1); // tseg2_min
                put_u32(&mut resp, 20, 128); // tseg2_max
                put_u32(&mut resp, 24, 128); // sjw_max
                put_u32(&mut resp, 28, 1); // brp_min
                put_u32(&mut resp, 32, 512); // brp_max
                put_u32(&mut resp, 36, 1); // brp_inc
                Some(InResponse::Accepted(&buf[..resp.len()]))
            }
            BREQ_GET_STATE => {
                let mut resp = [0u8; 12];
                put_u32(&mut resp, 0, 0); // GS_CAN_STATE_ERROR_ACTIVE
                put_u32(&mut resp, 4, 0); // rxerr
                put_u32(&mut resp, 8, 0); // txerr
                Some(InResponse::Accepted(&buf[..resp.len()]))
            }
            _ => None,
        }
    }

    fn control_out(&mut self, req: Request, data: &[u8]) -> Option<OutResponse> {
        if (req.request_type, req.recipient) != (RequestType::Vendor, Recipient::Interface) {
            return None;
        }
        let _ = self.iface;
        match req.request {
            BREQ_HOST_FORMAT => Some(OutResponse::Accepted),
            BREQ_BITTIMING | BREQ_DATA_BITTIMING => Some(OutResponse::Accepted),
            BREQ_MODE => {
                // struct gs_device_mode { u32 mode; u32 flags; }
                if data.len() >= 4 {
                    let mode = get_u32(data, 0);
                    match mode {
                        MODE_START => STARTED.store(true, Ordering::Relaxed),
                        MODE_RESET => STARTED.store(false, Ordering::Relaxed),
                        _ => {}
                    }
                }
                Some(OutResponse::Accepted)
            }
            _ => None,
        }
    }
}

/// host -> device `gs_host_frame` (classic CAN, 20 bytes).
fn parse_host_frame(buf: &[u8]) -> Option<FdFrame> {
    if buf.len() < 20 {
        return None;
    }
    let _echo_id = get_u32(buf, 0);
    let can_id = get_u32(buf, 4);
    let can_dlc = buf[8] as usize;
    let _channel = buf[9];
    let _flags = buf[10];
    if can_dlc > 8 {
        return None; // classic scope
    }
    if can_id & CAN_RTR_FLAG != 0 {
        return None; // remote frames unsupported
    }
    let data = &buf[12..12 + can_dlc];
    if can_id & CAN_EFF_FLAG != 0 {
        FdFrame::new_extended(can_id & 0x1FFF_FFFF, data).ok()
    } else {
        FdFrame::new_standard((can_id & 0x7FF) as u16, data).ok()
    }
}

/// device -> host `gs_host_frame` (classic CAN, 20 bytes).
fn build_rx_frame(env: &FdEnvelope) -> Option<[u8; 20]> {
    let frame = &env.frame;
    let header = frame.header();
    if header.fdcan() || header.rtr() {
        return None; // classic scope
    }
    let mut pkt = [0u8; 20];
    put_u32(&mut pkt, 0, ECHO_ID_RX);
    let can_id = match header.id() {
        Id::Standard(id) => id.as_raw() as u32,
        Id::Extended(id) => id.as_raw() | CAN_EFF_FLAG,
    };
    put_u32(&mut pkt, 4, can_id);
    pkt[8] = header.len();
    pkt[9] = 0; // channel
    pkt[10] = 0; // flags
    pkt[11] = 0; // reserved
    let len = header.len() as usize;
    pkt[12..12 + len].copy_from_slice(frame.data());
    Some(pkt)
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
