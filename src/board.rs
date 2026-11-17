use core::num::{NonZeroU8, NonZeroU16};

use embassy_stm32::{
    Config, Peri, Peripherals, bind_interrupts,
    can::{
        Can, CanConfigurator, IT0InterruptHandler, IT1InterruptHandler,
        config::{DataBitTiming, FrameTransmissionConfig, NominalBitTiming},
    },
    peripherals,
    rcc::{
        Hse, HseMode, Hsi48Config, Pll, PllDiv, PllMul, PllPreDiv, PllSource,
        mux::{Fdcansel, Usbsel},
    },
    time::Hertz,
    usb::{self, Driver},
};
use static_cell::StaticCell;

pub type Usb = Driver<'static, peripherals::USB_OTG_HS>;

bind_interrupts!(struct UsbIrqs {
    OTG_HS => usb::InterruptHandler<peripherals::USB_OTG_HS>;
});

bind_interrupts!(struct CanIrqs {
    FDCAN1_IT0 => IT0InterruptHandler<peripherals::FDCAN1>;
    FDCAN1_IT1 => IT1InterruptHandler<peripherals::FDCAN1>;
});

/// The clock tree is provided. Keep SYSCLK at its default HSI setting.
pub fn init() -> Peripherals {
    let mut config = Config::default();
    config.rcc.hse = Some(Hse {
        freq: Hertz(24_000_000),
        mode: HseMode::Oscillator,
    });
    config.rcc.pll2 = Some(Pll {
        source: PllSource::HSE,
        prediv: PllPreDiv::DIV12,
        mul: PllMul::MUL240,
        divp: Some(PllDiv::DIV2),
        divq: Some(PllDiv::DIV6),
        divr: Some(PllDiv::DIV2),
    });
    config.rcc.mux.fdcansel = Fdcansel::PLL2_Q;
    config.rcc.hsi48 = Some(Hsi48Config {
        sync_from_usb: true,
    });
    config.rcc.mux.usbsel = Usbsel::HSI48;
    embassy_stm32::init(config)
}

pub fn usb(
    otg: embassy_stm32::Peri<'static, peripherals::USB_OTG_HS>,
    dp: embassy_stm32::Peri<'static, peripherals::PA12>,
    dm: embassy_stm32::Peri<'static, peripherals::PA11>,
) -> Usb {
    static OUT: StaticCell<[u8; 512]> = StaticCell::new();
    let mut config = usb::Config::default();
    config.vbus_detection = false;
    Driver::new_fs(otg, UsbIrqs, dp, dm, OUT.init([0; 512]), config)
}

/// Provided CAN1 setup for the onboard transceiver and 80 MHz FDCAN clock.
/// This is the original "Classical CAN" configuration (ClassicCanOnly, 1 Mbit/s).
/// Kept for the Classical-CAN extension task; `can1_fd` below is the active config
/// used by `wiring.rs` because a CAN-FD node also carries classic frames.
#[allow(dead_code)]
pub fn can1(
    fdcan: Peri<'static, peripherals::FDCAN1>,
    rx: Peri<'static, peripherals::PD0>,
    tx: Peri<'static, peripherals::PD1>,
) -> Can<'static> {
    let mut can = CanConfigurator::new(fdcan, rx, tx, CanIrqs);
    // 1 Mbit/s, 75% sample point. Segment lengths are in time quanta.
    let timing = NominalBitTiming {
        prescaler: NonZeroU16::new(1).unwrap(),
        seg1: NonZeroU8::new(59).unwrap(),
        seg2: NonZeroU8::new(20).unwrap(),
        sync_jump_width: NonZeroU8::new(20).unwrap(),
    };
    let config = can
        .config()
        .set_nominal_bit_timing(timing)
        .set_frame_transmit(FrameTransmissionConfig::ClassicCanOnly);
    can.set_config(config);
    can.into_normal_mode()
}

/// CAN1 setup for CAN FD with Bit Rate Switching (BRS) and Transmitter Delay
/// Compensation (TDC). Arbitration (nominal) phase 1 Mbit/s, data phase 5 Mbit/s.
pub fn can1_fd(
    fdcan: Peri<'static, peripherals::FDCAN1>,
    rx: Peri<'static, peripherals::PD0>,
    tx: Peri<'static, peripherals::PD1>,
) -> Can<'static> {
    let mut can = CanConfigurator::new(fdcan, rx, tx, CanIrqs);
    // Nominal (arbitration) bit timing: 1 Mbit/s, 75% sample point.
    // FDCAN kernel clock is 80 MHz; prescaler 1 -> tq = 12.5 ns.
    // Bit time = 1 + 59 + 20 = 80 tq = 1000 ns = 1 Mbit/s; sample point = 60/80 = 75%.
    let nominal = NominalBitTiming {
        prescaler: NonZeroU16::new(1).unwrap(),
        seg1: NonZeroU8::new(59).unwrap(),
        seg2: NonZeroU8::new(20).unwrap(),
        sync_jump_width: NonZeroU8::new(20).unwrap(),
    };
    // Data (BRS) bit timing: 5 Mbit/s, 75% sample point.
    // prescaler 1 -> tq = 12.5 ns; bit time = 1 + 11 + 4 = 16 tq = 200 ns = 5 Mbit/s;
    // sample point = 12/16 = 75%.
    let data = DataBitTiming {
        transceiver_delay_compensation: true,
        prescaler: NonZeroU16::new(1).unwrap(),
        seg1: NonZeroU8::new(11).unwrap(),
        seg2: NonZeroU8::new(4).unwrap(),
        sync_jump_width: NonZeroU8::new(4).unwrap(),
    };
    let config = can
        .config()
        .set_nominal_bit_timing(nominal)
        .set_data_bit_timing(data)
        .set_frame_transmit(FrameTransmissionConfig::AllowFdCanAndBRS);
    can.set_config(config);
    // The Embassy 0.6.0 FDCAN driver does not program the TDCR register, so write it
    // directly via the PAC while the peripheral is still in configuration mode
    // (CCE=1, INIT=1). TDCO is in units of mtq (1 FDCAN kernel clock period = 12.5 ns);
    // 10 mtq = 125 ns covers the MCP2542FD TX->RX loop delay. TDCF = 0 (no filter window).
    stm32_metapac::FDCAN1.tdcr().write(|w| {
        w.set_tdco(10);
        w.set_tdcf(0);
    });
    can.into_normal_mode()
}
