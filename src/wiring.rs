use embassy_stm32::{
    bind_interrupts,
    can::{BufferedFdCanReceiver, BufferedFdCanSender, RxFdBuf, TxFdBuf},
    gpio::{Flex, Level, Output, Speed},
    peripherals,
    usart::{self, BufferedUart},
    Peripherals,
};
use static_cell::StaticCell;

use crate::swd::Swd;

pub struct Ports {
    pub can_tx: BufferedFdCanSender,
    pub can_rx: BufferedFdCanReceiver,
    pub uart: Option<BufferedUart<'static>>,
}

const CAN_TX_BUF: usize = 32;
const CAN_RX_BUF: usize = 32;

// UART7 中断（独立 3-pin 串口排针：GND / TX=PE8 / RX=PE7）
bind_interrupts!(struct Uart7Irqs {
    UART7 => usart::BufferedInterruptHandler<peripherals::UART7>;
});

pub fn configure(p: Peripherals) -> (crate::board::Usb, Ports, Option<Swd>) {
    let usb = crate::board::usb(p.USB_OTG_HS, p.PA12, p.PA11);

    // SWD：PE13(SWCLK) / PE9(SWDIO)，保持你已验证可用的配置
    let swclk = Output::new(p.PE13, Level::Low, Speed::Low);
    let swdio = Flex::new(p.PE9);
    let swd = Some(Swd::new(swclk, swdio));

    // UART7：115200 8N1，中断 + 静态收发缓冲区
    let mut config = usart::Config::default();
    config.baudrate = 115_200;
    config.data_bits = usart::DataBits::DataBits8;
    config.parity = usart::Parity::ParityNone;
    config.stop_bits = usart::StopBits::STOP1;

    static TX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();

    // 参数顺序：外设, RX 引脚, TX 引脚, TX 缓冲, RX 缓冲, 中断, 配置
    // UART7：TX = PE8，RX = PE7
    let uart = BufferedUart::new(
        p.UART7,
        p.PE7, // UART7_RX
        p.PE8, // UART7_TX
        TX_BUF.init([0; 256]),
        RX_BUF.init([0; 256]),
        Uart7Irqs,
        config,
    )
    .unwrap();

    // CAN：改用 CAN FD + BRS + TDC（1 Mbit/s 仲裁段 / 5 Mbit/s 数据段），
    // 并用 buffered_fd 拆成可共享的 can_tx / can_rx（控制串口桥接 + gs_usb 共用）。
    let can = crate::board::can1_fd(p.FDCAN1, p.PD0, p.PD1);
    static CAN_TX_QUEUE: StaticCell<TxFdBuf<CAN_TX_BUF>> = StaticCell::new();
    static CAN_RX_QUEUE: StaticCell<RxFdBuf<CAN_RX_BUF>> = StaticCell::new();
    let can = can.buffered_fd(
        CAN_TX_QUEUE.init(TxFdBuf::new()),
        CAN_RX_QUEUE.init(RxFdBuf::new()),
    );
    let can_tx = can.writer();
    let can_rx = can.reader();
    core::mem::forget(can);

    (usb, Ports { can_tx, can_rx, uart: Some(uart) }, swd)
}
