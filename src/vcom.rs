use crate::setup::{Irqs, VcomResources};
use crate::usb::{UsbDriver, MAX_PACKET_SIZE};
use embassy_futures::select::{select, Either};
use embassy_rp::gpio::Pin;
use embassy_rp::uart::{self, BufferedUart, BufferedUartRx, BufferedUartTx};
use embassy_usb::class::cdc_acm::{self, CdcAcmClass, ControlChanged, LineCoding, ParityType};
use embedded_io_async::{Read, Write};
use static_cell::StaticCell;

const TX_BUFFER_SIZE: usize = 512;
const RX_BUFFER_SIZE: usize = 4096;

/// Lowest supported baud rate. Rebuilding the UART waits 15 bit times of the old baud rate,
/// which must not stall USB for too long.
const MIN_BAUDRATE: u32 = 300;

/// Bridges the virtual COM port to the target UART.
///
/// The UART is rebuilt whenever the host changes the line coding, as only the baud rate can be
/// changed on a running `BufferedUart`.
#[embassy_executor::task]
pub async fn run(class: CdcAcmClass<'static, UsbDriver>, resources: VcomResources) -> ! {
    static TX_BUFFER: StaticCell<[u8; TX_BUFFER_SIZE]> = StaticCell::new();
    static RX_BUFFER: StaticCell<[u8; RX_BUFFER_SIZE]> = StaticCell::new();
    let tx_buffer = TX_BUFFER.init([0; TX_BUFFER_SIZE]);
    let rx_buffer = RX_BUFFER.init([0; RX_BUFFER_SIZE]);

    // The direction pins live as long as the task, dropping them would reset the pins.
    let VcomResources {
        mut uart,
        mut tx,
        mut rx,
        mut dir_tx,
        dir_rx: _dir_rx,
    } = resources;
    let rx_pin = rx.pin();

    let (mut sender, mut receiver, control) = class.split_with_control();
    let mut config = uart::Config::default();

    loop {
        let mut uart = BufferedUart::new(
            uart.reborrow(),
            tx.reborrow(),
            rx.reborrow(),
            Irqs,
            &mut tx_buffer[..],
            &mut rx_buffer[..],
            config,
        );
        // The RX translator floats while the target is unpowered, keep the line idle.
        embassy_rp::pac::PADS_BANK0
            .gpio(rx_pin as usize)
            .modify(|w| {
                w.set_pue(true);
                w.set_pde(false);
            });
        // Only drive the target once the UART idles the TX pin high, anything else is a break.
        dir_tx.set_high();
        defmt::info!("VCOM baud rate: {}", config.baudrate);

        let (uart_tx, uart_rx) = uart.split_ref();
        config = match select(
            usb_to_uart(&mut receiver, uart_tx),
            uart_to_usb(uart_rx, &mut sender, &control, config),
        )
        .await
        {
            Either::First(never) => never,
            Either::Second(new_config) => new_config,
        };
    }
}

async fn usb_to_uart(
    receiver: &mut cdc_acm::Receiver<'static, UsbDriver>,
    uart: &mut BufferedUartTx,
) -> ! {
    let mut buf = [0; MAX_PACKET_SIZE as usize];

    loop {
        if let Ok(n) = receiver.read_packet(&mut buf).await {
            if let Err(e) = uart.write_all(&buf[..n]).await {
                defmt::warn!("VCOM TX error: {}", e);
            }
        }
    }
}

/// Forwards the UART data to the host, until the host requests a different line coding.
///
/// Only the UART read races the control changes, so data that was read is never lost when the
/// host toggles DTR or RTS.
async fn uart_to_usb(
    uart: &mut BufferedUartRx,
    sender: &mut cdc_acm::Sender<'static, UsbDriver>,
    control: &ControlChanged<'static>,
    config: uart::Config,
) -> uart::Config {
    // One byte less than a full packet, so every transfer ends with a short packet and needs no
    // zero length packet.
    let mut buf = [0; MAX_PACKET_SIZE as usize - 1];

    loop {
        match select(uart.read(&mut buf), control.control_changed()).await {
            // Without an open terminal the data is dropped, instead of arriving stale later on.
            Either::First(Ok(n)) => {
                if control.dtr() {
                    sender.write_packet(&buf[..n]).await.ok();
                }
            }
            Either::First(Err(e)) => defmt::debug!("VCOM RX error: {}", e),
            Either::Second(()) => {
                if let Some(new_config) = uart_config(&sender.line_coding()) {
                    if new_config != config {
                        return new_config;
                    }
                }
            }
        }
    }
}

/// Maps the line coding requested by the host to a UART configuration, if the UART supports it.
fn uart_config(line_coding: &LineCoding) -> Option<uart::Config> {
    let default = LineCoding::default();
    let is_default = line_coding.data_rate() == default.data_rate()
        && line_coding.data_bits() == default.data_bits()
        && line_coding.parity_type() == default.parity_type()
        && line_coding.stop_bits() == default.stop_bits();
    // embassy-usb sets this at power up and on every bus reset, it is not a host request.
    if is_default {
        return None;
    }

    let clk_peri = embassy_rp::clocks::clk_peri_freq();
    let baudrate = line_coding.data_rate();
    if baudrate < MIN_BAUDRATE || baudrate > clk_peri / 16 {
        defmt::warn!("Unsupported VCOM baud rate: {}", baudrate);
        return None;
    }

    let data_bits = match line_coding.data_bits() {
        5 => uart::DataBits::DataBits5,
        6 => uart::DataBits::DataBits6,
        7 => uart::DataBits::DataBits7,
        8 => uart::DataBits::DataBits8,
        _ => return None,
    };

    let parity = match line_coding.parity_type() {
        ParityType::None => uart::Parity::ParityNone,
        ParityType::Odd => uart::Parity::ParityOdd,
        ParityType::Even => uart::Parity::ParityEven,
        ParityType::Mark | ParityType::Space => return None,
    };

    // The UART cannot do 1.5 stop bits, 2 is the closest match.
    let stop_bits = match line_coding.stop_bits() {
        cdc_acm::StopBits::One => uart::StopBits::STOP1,
        cdc_acm::StopBits::OnePointFive | cdc_acm::StopBits::Two => uart::StopBits::STOP2,
    };

    let mut config = uart::Config::default();
    config.baudrate = baudrate;
    config.data_bits = data_bits;
    config.parity = parity;
    config.stop_bits = stop_bits;
    Some(config)
}
