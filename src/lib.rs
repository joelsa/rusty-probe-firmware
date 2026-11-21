#![no_std]

use panic_probe as _;

use defmt_brtt as _;

pub mod dap;
pub mod device_signature;
pub mod leds;
pub mod setup;
pub mod systick_delay;
pub mod usb;
pub mod vcom;

defmt::timestamp! {"{=u64:us}", {
    embassy_time::Instant::now().as_micros()
}
}
