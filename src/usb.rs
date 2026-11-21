use crate::dap::{Response, DAP_PACKET_SIZE, REQUESTS, RESPONSES};
use crate::setup::Irqs;
use core::future::Future;
use dap_rs::usb::Request;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_rp::Peri;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_usb::class::cdc_acm::{self, CdcAcmClass};
use embassy_usb::class::hid::{self, HidReaderWriter};
use embassy_usb::driver::{EndpointError, EndpointIn, EndpointOut};
use embassy_usb::types::StringIndex;
use embassy_usb::{msos, Builder, Config, Handler, UsbDevice};
use static_cell::StaticCell;

/// Signalled when the host resets, suspends or unconfigures the device, which ends the DAP session.
static USB_INACTIVE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub type UsbDriver = Driver<'static, USB>;
pub type DapV1 = HidReaderWriter<'static, UsbDriver, DAP_PACKET_SIZE, DAP_PACKET_SIZE>;

pub const MAX_PACKET_SIZE: u16 = 64;

const MANUFACTURER: &str = "Probe-rs development team";
const PRODUCT: &str = "Rusty Probe with CMSIS-DAP v1/v2 Support";

/// Vendor request code the host uses to fetch the Microsoft OS 2.0 descriptors.
const MSOS_VENDOR_CODE: u8 = 0x41;

/// CMSIS-DAP v2 interface GUID, from https://arm-software.github.io/CMSIS_5/DAP/html/group__DAP__ConfigUSB__gr.html
const DAP_V2_INTERFACE_GUID: &str = "{CDB3B5AD-293B-4663-AA36-1AAE46463776}";

const DAP_V1_REPORT_DESCRIPTOR: &[u8] = &[
    0x06, 0x00, 0xFF, // Usage Page (Vendor Defined 0xFF00)
    0x09, 0x01, // Usage (0x01)
    0xA1, 0x01, // Collection (Application)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0xFF, //   Logical Maximum (255)
    0x75, 0x08, //   Report Size (8)
    0x95, 0x40, //   Report Count (64)
    0x09, 0x01, //   Usage (0x01)
    0x81, 0x02, //   Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x40, //   Report Count (64)
    0x09, 0x01, //   Usage (0x01)
    0x91,
    0x02, //   Output (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0x95, 0x01, //   Report Count (1)
    0x09, 0x01, //   Usage (0x01)
    0xB1,
    0x02, //   Feature (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position,Non-volatile)
    0xC0, // End Collection
];

/// Implements the CMSIS DAP descriptors, the virtual COM port and the optional debug port.
pub struct ProbeUsb {
    pub device: UsbDevice<'static, UsbDriver>,
    pub dap_v1: DapV1,
    pub dap_v2: CmsisDapV2,
    pub vcom: CdcAcmClass<'static, UsbDriver>,
    /// Carries the `defmt` logs and the reboot command, so they never mix with the target's UART.
    #[cfg(any(feature = "defmt-bbq", feature = "usb-serial-reboot"))]
    pub debug: CdcAcmClass<'static, UsbDriver>,
}

impl ProbeUsb {
    pub fn new(usb: Peri<'static, USB>, serial_number: &'static str) -> Self {
        static CONFIG_DESCRIPTOR: StaticCell<[u8; 512]> = StaticCell::new();
        static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
        static MSOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
        // Also holds the string descriptors, `PRODUCT` alone needs 82 bytes.
        static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();
        static DAP_V1_STATE: StaticCell<hid::State> = StaticCell::new();
        static DAP_V2_HANDLER: StaticCell<DapV2Handler> = StaticCell::new();
        static VCOM_STATE: StaticCell<cdc_acm::State> = StaticCell::new();
        #[cfg(any(feature = "defmt-bbq", feature = "usb-serial-reboot"))]
        static DEBUG_STATE: StaticCell<cdc_acm::State> = StaticCell::new();

        let mut config = Config::new(0x1209, 0x4853);
        config.manufacturer = Some(MANUFACTURER);
        config.product = Some(PRODUCT);
        config.serial_number = Some(serial_number);
        config.max_power = 500;
        config.max_packet_size_0 = 64;

        let mut builder = Builder::new(
            Driver::new(usb, Irqs),
            config,
            CONFIG_DESCRIPTOR.init([0; 512]),
            BOS_DESCRIPTOR.init([0; 256]),
            MSOS_DESCRIPTOR.init([0; 256]),
            CONTROL_BUF.init([0; 128]),
        );
        builder.msos_descriptor(msos::windows_version::WIN8_1, MSOS_VENDOR_CODE);

        // Added first, so it enumerates as the first serial port (`/dev/ttyACM0`), where `xtask`
        // looks for it.
        #[cfg(any(feature = "defmt-bbq", feature = "usb-serial-reboot"))]
        let debug = CdcAcmClass::new(
            &mut builder,
            DEBUG_STATE.init(cdc_acm::State::new()),
            MAX_PACKET_SIZE,
        );

        let dap_v2 = CmsisDapV2::new(&mut builder, &DAP_V2_HANDLER);

        let dap_v1 = HidReaderWriter::new(
            &mut builder,
            DAP_V1_STATE.init(hid::State::new()),
            hid::Config {
                report_descriptor: DAP_V1_REPORT_DESCRIPTOR,
                request_handler: None,
                poll_ms: 1,
                max_packet_size: MAX_PACKET_SIZE,
                hid_subclass: hid::HidSubclass::No,
                hid_boot_protocol: hid::HidBootProtocol::None,
            },
        );

        let vcom = CdcAcmClass::new(
            &mut builder,
            VCOM_STATE.init(cdc_acm::State::new()),
            MAX_PACKET_SIZE,
        );

        ProbeUsb {
            device: builder.build(),
            dap_v1,
            dap_v2,
            vcom,
            #[cfg(any(feature = "defmt-bbq", feature = "usb-serial-reboot"))]
            debug,
        }
    }
}

struct DapV2Handler {
    interface: StringIndex,
}

impl Handler for DapV2Handler {
    fn reset(&mut self) {
        USB_INACTIVE.signal(());
    }

    fn configured(&mut self, configured: bool) {
        if !configured {
            USB_INACTIVE.signal(());
        }
    }

    fn suspended(&mut self, suspended: bool) {
        if suspended {
            USB_INACTIVE.signal(());
        }
    }

    fn get_string(&mut self, index: StringIndex, _lang_id: u16) -> Option<&str> {
        (index == self.interface).then_some("CMSIS-DAP v2 Interface")
    }
}

/// CMSIS-DAP v2 bulk interface.
///
/// Unlike `embassy_usb::class::cmsis_dap_v2`, every packet is one command. Hosts send a command
/// filling the whole packet without a trailing zero length packet, which that class would wait for.
pub struct CmsisDapV2 {
    read_ep: <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointOut,
    write_ep: <UsbDriver as embassy_usb::driver::Driver<'static>>::EndpointIn,
}

impl CmsisDapV2 {
    fn new(
        builder: &mut Builder<'static, UsbDriver>,
        handler: &'static StaticCell<DapV2Handler>,
    ) -> Self {
        let interface_string = builder.string();
        let mut function = builder.function(0xFF, 0, 0);
        function.msos_feature(msos::CompatibleIdFeatureDescriptor::new("WINUSB", ""));
        function.msos_feature(msos::RegistryPropertyFeatureDescriptor::new(
            "DeviceInterfaceGUIDs",
            msos::PropertyData::RegMultiSz(&[DAP_V2_INTERFACE_GUID]),
        ));
        let mut interface = function.interface();
        let mut alt = interface.alt_setting(0xFF, 0, 0, Some(interface_string));
        let read_ep = alt.endpoint_bulk_out(None, MAX_PACKET_SIZE);
        let write_ep = alt.endpoint_bulk_in(None, MAX_PACKET_SIZE);
        drop(function);

        builder.handler(handler.init(DapV2Handler {
            interface: interface_string,
        }));

        CmsisDapV2 { read_ep, write_ep }
    }
}

#[embassy_executor::task]
pub async fn run(mut device: UsbDevice<'static, UsbDriver>) -> ! {
    device.run().await
}

/// Forwards DAP commands between the USB interfaces and the DAP engine on core 1.
///
/// Only the reads are raced, a command that was read is always processed, so requests and
/// responses never get out of step.
#[embassy_executor::task]
pub async fn dap_bridge(mut dap_v1: DapV1, mut dap_v2: CmsisDapV2) -> ! {
    let mut v1_report = [0; DAP_PACKET_SIZE];
    let mut v2_packet = [0; DAP_PACKET_SIZE];

    loop {
        match select3(
            USB_INACTIVE.wait(),
            dap_v1.read(&mut v1_report),
            dap_v2.read_ep.read(&mut v2_packet),
        )
        .await
        {
            Either3::First(()) => suspend().await,
            Either3::Second(Ok(n)) => {
                let response = process(Request::DAP1Command((v1_report, n))).await;
                if response.len > 0 {
                    // HID reports always have the full report size.
                    reply(dap_v1.write(&response.data)).await;
                }
            }
            Either3::Third(Ok(n)) => {
                let response = process(Request::DAP2Command((v2_packet, n))).await;
                if response.len > 0 {
                    reply(dap_v2.write_ep.write(&response.data[..response.len])).await;
                }
            }
            Either3::Second(Err(_)) | Either3::Third(Err(_)) => {}
        }
    }
}

async fn process(request: Request) -> Response {
    REQUESTS.send(request).await;
    RESPONSES.receive().await
}

/// Sends a response, unless the host resets the device first. After a reset the response would
/// wrongly answer the next command.
async fn reply(write: impl Future<Output = Result<(), EndpointError>>) {
    if let Either::First(()) = select(USB_INACTIVE.wait(), write).await {
        suspend().await;
    }
}

async fn suspend() {
    defmt::info!("USB reset or suspended, suspending DAP");
    REQUESTS.send(Request::Suspend).await;
}

/// Sends the `defmt` logs over the debug port.
#[cfg(feature = "defmt-bbq")]
#[embassy_executor::task]
pub async fn log_pump(
    mut sender: cdc_acm::Sender<'static, UsbDriver>,
    mut consumer: defmt_brtt::DefmtConsumer,
) -> ! {
    loop {
        sender.wait_connection().await;
        while send_logs(&mut sender, &mut consumer).await.is_ok() {}
    }
}

#[cfg(feature = "defmt-bbq")]
async fn send_logs(
    sender: &mut cdc_acm::Sender<'static, UsbDriver>,
    consumer: &mut defmt_brtt::DefmtConsumer,
) -> Result<(), EndpointError> {
    let grant = consumer.wait_for_log().await;
    // Short packets end every transfer, so no zero length packets are needed.
    let len = grant.len().min(MAX_PACKET_SIZE as usize - 1);
    sender.write_packet(&grant[..len]).await?;
    grant.release(len);
    Ok(())
}

/// Reboots into the USB bootloader when the magic sequence arrives on the debug port.
#[cfg(feature = "usb-serial-reboot")]
#[embassy_executor::task]
pub async fn reboot_listener(mut receiver: cdc_acm::Receiver<'static, UsbDriver>) -> ! {
    let mut buf = [0; MAX_PACKET_SIZE as usize];

    loop {
        receiver.wait_connection().await;
        while let Ok(n) = receiver.read_packet(&mut buf).await {
            if buf[..n] == 0xDABAD000u32.to_be_bytes() {
                embassy_rp::rom_data::reset_to_usb_boot(0, 0);
            }
        }
    }
}
