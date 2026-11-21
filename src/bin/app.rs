#![no_std]
#![no_main]

use defmt::unwrap;
use embassy_executor::Spawner;
use embassy_rp::executor::Executor;
use embassy_rp::multicore::{spawn_core1, Stack};
use embassy_time::Timer;
use rusty_probe::{
    dap,
    leds::{LedManager, Vtarget},
    setup::*,
    usb::{self, ProbeUsb},
    vcom,
};
use static_cell::StaticCell;

static CORE1_STACK: StaticCell<Stack<8192>> = StaticCell::new();
static CORE1_EXECUTOR: StaticCell<Executor> = StaticCell::new();

// The `unwrap!`s below can only fail on a second call or spawn, which never happens.
#[embassy_executor::main(
    executor = "embassy_rp::executor::Executor",
    entry = "cortex_m_rt::entry"
)]
async fn main(spawner: Spawner) {
    #[cfg(feature = "defmt-bbq")]
    let defmt_consumer = unwrap!(defmt_brtt::init!().ok());

    let probe = setup(
        embassy_rp::init(Default::default()),
        unwrap!(cortex_m::Peripherals::take()),
    );
    let probe_usb = ProbeUsb::new(probe.usb, probe.serial_number);

    // Bit-banging SWD on its own core keeps USB and the virtual COM port responsive.
    let dap_resources = probe.dap;
    spawn_core1(probe.core1, CORE1_STACK.init(Stack::new()), move || {
        let executor = CORE1_EXECUTOR.init(Executor::new());
        executor.run(|spawner| spawner.spawn(unwrap!(dap::engine(dap_resources))))
    });

    spawner.spawn(unwrap!(usb::run(probe_usb.device)));
    spawner.spawn(unwrap!(usb::dap_bridge(probe_usb.dap_v1, probe_usb.dap_v2)));
    spawner.spawn(unwrap!(vcom::run(probe_usb.vcom, probe.vcom)));
    spawner.spawn(unwrap!(led_driver(probe.leds)));
    spawner.spawn(unwrap!(voltage_translator_control(
        probe.target_vcc,
        probe.translator_power,
        probe.target_power,
        probe.target_physically_connected,
    )));

    #[cfg(any(feature = "defmt-bbq", feature = "usb-serial-reboot"))]
    {
        let (_log_sender, _reboot_receiver) = probe_usb.debug.split();

        #[cfg(feature = "defmt-bbq")]
        spawner.spawn(unwrap!(usb::log_pump(_log_sender, defmt_consumer)));

        #[cfg(feature = "usb-serial-reboot")]
        spawner.spawn(unwrap!(usb::reboot_listener(_reboot_receiver)));
    }
}

#[embassy_executor::task]
async fn led_driver(mut leds: LedManager) {
    leds.run().await;
}

#[embassy_executor::task]
async fn voltage_translator_control(
    mut target_vcc: TargetVccReader,
    mut translator_power: TranslatorPower,
    // Owned by the task to keep the target power pins configured.
    _target_power: TargetPower,
    mut target_physically_connected: TargetPhysicallyConnected,
) {
    loop {
        // Set the voltage translators to track Target's VCC
        let target_vcc_mv = target_vcc.read_voltage_mv();

        if target_vcc_mv > 1500 {
            translator_power.set_translator_vcc(target_vcc_mv);
        } else {
            if target_physically_connected.target_detected() {
                // If there is no VCC detected use 3.3v.
                translator_power.set_translator_vcc(3300);
            } else {
                translator_power.set_translator_vcc(0);
            }
        }

        defmt::trace!("Tracking Target VCC at {} mV", target_vcc_mv);

        if target_vcc_mv > 2500 {
            LedManager::set_current_vtarget(Some(Vtarget::Voltage3V3));
        } else if target_vcc_mv > 1500 {
            LedManager::set_current_vtarget(Some(Vtarget::Voltage1V8));
        } else {
            if target_physically_connected.target_detected() {
                // If there is no VCC detected use 3.3v.
                LedManager::set_current_vtarget(Some(Vtarget::Voltage3V3));
            } else {
                LedManager::set_current_vtarget(None);
            }
        };

        Timer::after_millis(100).await;
    }
}
