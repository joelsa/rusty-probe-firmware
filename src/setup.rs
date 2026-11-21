use crate::dap::{Context, Jtag, Swd, Swo, Wait};
use crate::leds::{BoardLeds, HostStatusToken, LedManager};
use cortex_m::peripheral::SYST;
use embassy_rp::adc::{self, Adc, Channel};
use embassy_rp::gpio::{Drive, Flex, Input, Level, Output, Pull, SlewRate};
use embassy_rp::peripherals::{
    CORE1, PIN_0, PIN_20, PIN_21, PIN_5, PWM_SLICE0, PWM_SLICE2, UART1, USB,
};
use embassy_rp::pwm::{self, Pwm};
use embassy_rp::{bind_interrupts, rom_data, uart, usb, Peri, Peripherals};

bind_interrupts!(pub struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
    UART1_IRQ => uart::BufferedInterruptHandler<UART1>;
});

pub type DapHandler = dap_rs::dap::Dap<'static, Context, HostStatusToken, Wait, Jtag, Swd, Swo>;

pub type ResetPin = Flex<'static>;
pub type SwdioPin = Flex<'static>;
pub type SwclkPin = Flex<'static>;
pub type DirSwdioPin = Output<'static>;
pub type DirSwclkPin = Output<'static>;
pub type VcomUart = UART1;
pub type VcomTxPin = PIN_20;
pub type VcomRxPin = PIN_21;

/// Everything core 1 needs to process DAP commands.
pub struct DapResources {
    pub swdio: SwdioPin,
    pub swclk: SwclkPin,
    pub nreset: ResetPin,
    pub dir_swdio: DirSwdioPin,
    pub dir_swclk: DirSwclkPin,
    pub systick: SYST,
    pub host_status: HostStatusToken,
}

/// The UART bridged to the virtual COM port, and the direction pins of its voltage translators.
pub struct VcomResources {
    pub uart: Peri<'static, VcomUart>,
    pub tx: Peri<'static, VcomTxPin>,
    pub rx: Peri<'static, VcomRxPin>,
    pub dir_tx: Output<'static>,
    pub dir_rx: Output<'static>,
}

pub struct Probe {
    pub core1: Peri<'static, CORE1>,
    pub usb: Peri<'static, USB>,
    pub serial_number: &'static str,
    pub dap: DapResources,
    pub vcom: VcomResources,
    pub leds: LedManager,
    pub target_vcc: TargetVccReader,
    pub translator_power: TranslatorPower,
    pub target_power: TargetPower,
    pub target_physically_connected: TargetPhysicallyConnected,
}

#[inline(always)]
pub fn setup(p: Peripherals, core: cortex_m::Peripherals) -> Probe {
    let rom_version = rom_data::rom_version_number();
    let git_revision = rom_data::git_revision();
    let copyright = rom_data::copyright_string();

    defmt::info!(
        "RP2040-B{=u8} (ROM {=u32:x}) {=str}",
        rom_version,
        git_revision,
        copyright
    );

    let serial_number = crate::device_signature::device_id_hex(p.FLASH);
    defmt::info!("Device ID: {}", serial_number);

    let mut led_green = Output::new(p.PIN_27, Level::High);
    led_green.set_drive_strength(Drive::_4mA);
    let mut led_red = Output::new(p.PIN_28, Level::High);
    led_red.set_drive_strength(Drive::_4mA);
    let mut led_blue = Output::new(p.PIN_29, Level::High);
    led_blue.set_drive_strength(Drive::_4mA);

    // Configure GPIO26 as an ADC input
    let target_vcc = TargetVccReader {
        adc: Adc::new_blocking(p.ADC, adc::Config::default()),
        pin: Channel::new_pin(p.PIN_26, Pull::None),
    };

    ///////////////////////////////////
    // Voltage translator power
    ///////////////////////////////////

    let mut translator_power = TranslatorPower::new(p.PWM_SLICE2, p.PIN_5);

    translator_power.set_translator_vcc(1800);

    ///////////////////////////////////
    // Target power
    ///////////////////////////////////

    let mut target_power = TargetPower::new(
        Output::new(p.PIN_3, Level::Low),
        Output::new(p.PIN_7, Level::Low),
        Output::new(p.PIN_6, Level::Low),
        p.PWM_SLICE0,
        p.PIN_0,
    );

    // target_power.enable_vtgt();
    target_power.set_vtgt(1800);

    cortex_m::asm::delay(1_000_000);

    let mut io = Flex::new(p.PIN_10);
    let mut ck = Flex::new(p.PIN_11);
    let mut dir_io = Output::new(p.PIN_12, Level::Low);
    let mut dir_ck = Output::new(p.PIN_19, Level::Low);
    let mut reset = Flex::new(p.PIN_9);
    let mut gnd_detect = Input::new(p.PIN_8, Pull::Up);

    // Restore the pad reset defaults the drivers clear: Schmitt trigger on, 4 mA drive.
    io.set_schmitt(true);
    ck.set_schmitt(true);
    reset.set_schmitt(true);
    reset.set_drive_strength(Drive::_4mA);
    gnd_detect.set_schmitt(true);

    io.set_pull(Pull::Down);
    ck.set_pull(Pull::Down);
    reset.set_pull(Pull::None);

    // High speed IO
    io.set_drive_strength(Drive::_12mA);
    io.set_slew_rate(SlewRate::Fast);
    ck.set_drive_strength(Drive::_12mA);
    ck.set_slew_rate(SlewRate::Fast);
    dir_io.set_drive_strength(Drive::_12mA);
    dir_io.set_slew_rate(SlewRate::Fast);
    dir_ck.set_drive_strength(Drive::_12mA);
    dir_ck.set_slew_rate(SlewRate::Fast);

    // The RX translator always listens to the target. The TX translator starts listening too and
    // only drives the target once the UART owns the TX pin.
    let vcom = VcomResources {
        uart: p.UART1,
        tx: p.PIN_20,
        rx: p.PIN_21,
        dir_tx: Output::new(p.PIN_24, Level::Low),
        dir_rx: Output::new(p.PIN_25, Level::Low),
    };

    let board_leds = BoardLeds::new(led_red, led_green, led_blue);
    let mut led_manager = LedManager::new(board_leds);
    let host_status = led_manager.host_status_token();

    Probe {
        core1: p.CORE1,
        usb: p.USB,
        serial_number,
        dap: DapResources {
            swdio: io,
            swclk: ck,
            nreset: reset,
            dir_swdio: dir_io,
            dir_swclk: dir_ck,
            systick: core.SYST,
            host_status,
        },
        vcom,
        leds: led_manager,
        target_vcc,
        translator_power,
        target_power,
        target_physically_connected: TargetPhysicallyConnected { pin: gnd_detect },
    }
}

pub struct TargetPhysicallyConnected {
    pin: Input<'static>,
}

impl TargetPhysicallyConnected {
    /// This checks for the target being connected via the GND detect pin.
    pub fn target_detected(&mut self) -> bool {
        #[cfg(feature = "gnddetect")]
        return self.pin.is_low();
        #[cfg(not(feature = "gnddetect"))]
        return true;
    }
}

pub struct TargetVccReader {
    pub pin: Channel<'static>,
    pub adc: Adc<'static, adc::Blocking>,
}

impl TargetVccReader {
    pub fn read_voltage_mv(&mut self) -> u32 {
        let Self { pin, adc } = self;
        let val = match adc.blocking_read(pin) {
            Ok(val) => val,
            Err(e) => {
                defmt::warn!("Reading the target VCC failed: {}", e);
                0
            }
        };

        (2 * 3300 * val as u32) / 4095
    }
}

pub struct TranslatorPower {
    vtranslator_pwm: Pwm<'static>,
    config: pwm::Config,
}

impl TranslatorPower {
    pub fn new(slice: Peri<'static, PWM_SLICE2>, vtranslator_pin: Peri<'static, PIN_5>) -> Self {
        let pin_number = 5;

        // Output channel B on PWM2 to GPIO 5
        let mut config = pwm::Config::default();
        config.top = 4095;
        config.compare_b = 1023;
        let vtranslator_pwm = Pwm::new_output_b(slice, vtranslator_pin, config.clone());

        set_high_speed_pad(pin_number);

        Self {
            vtranslator_pwm,
            config,
        }
    }

    pub fn set_translator_vcc(&mut self, mv: u32) {
        // ans = 0.0900 * 4095
        let v33 = 369; // duty cycle that give 3.3v

        // ans = 0.7617 * 4095
        let v18 = 3119; // duty cycle that gives 1.8v

        let vomin = 1800; // The actual voltage when PWM is set to v18 duty cycle
        let vomax = 3300; // The actual voltage when PWM is set to v33 duty cycle

        let mv = mv.min(vomax).max(vomin);

        let limit_diff = v18 - v33;
        let mv_diff = mv - vomin;

        let cnt = ((vomax - vomin - mv_diff) * limit_diff) / (vomax - vomin) + v33;

        self.config.compare_b = cnt as u16;
        self.vtranslator_pwm.set_config(&self.config);
    }
}

pub struct TargetPower {
    enable_5v_key: Output<'static>,
    // Kept to hold the pin high, dropping it would reset the pin.
    _enable_5v: Output<'static>,
    enable_vtgt: Output<'static>,
    vtgt_pwm: Pwm<'static>,
    config: pwm::Config,
}

impl TargetPower {
    pub fn enable_vtgt(&mut self) {
        self.enable_vtgt.set_high();
    }

    pub fn enable_5v_key(&mut self) {
        self.enable_5v_key.set_high();
    }

    pub fn new(
        mut enable_5v_key: Output<'static>,
        mut enable_5v: Output<'static>,
        mut enable_vtgt: Output<'static>,
        slice: Peri<'static, PWM_SLICE0>,
        vtgt_pin: Peri<'static, PIN_0>,
    ) -> Self {
        let pin_number = 0;

        // Always enable the protected 5v (existed until rev E of hardware)
        enable_5v.set_high();

        enable_5v_key.set_low();
        enable_vtgt.set_low();

        // Output channel A on PWM0 to GPIO 0
        let mut config = pwm::Config::default();
        config.top = 4095;
        config.compare_a = 1023;
        let vtgt_pwm = Pwm::new_output_a(slice, vtgt_pin, config.clone());

        set_high_speed_pad(pin_number);

        Self {
            enable_5v_key,
            _enable_5v: enable_5v,
            enable_vtgt,
            vtgt_pwm,
            config,
        }
    }

    pub fn set_vtgt(&mut self, mv: u32) {
        let v33 = 369; // duty cycle that give 3.3v
        let v18 = 3119; // duty cycle that gives 1.8v

        let vomin = 1800; // The actual voltage when PWM is set to v18 duty cycle
        let vomax = 3300; // The actual voltage when PWM is set to v33 duty cycle

        let mv = mv.min(vomax).max(vomin);

        let limit_diff = v18 - v33;
        let mv_diff = mv - vomin;

        let cnt = ((vomax - vomin - mv_diff) * limit_diff) / (vomax - vomin) + v33;

        self.config.compare_a = cnt as u16;
        self.vtgt_pwm.set_config(&self.config);
    }
}

/// The PWM driver does not expose the pad settings, so set them like the other high speed IO.
fn set_high_speed_pad(pin_number: usize) {
    embassy_rp::pac::PADS_BANK0.gpio(pin_number).modify(|w| {
        w.set_drive(embassy_rp::pac::pads::vals::Drive::_12M_A);
        w.set_slewfast(true);
    });
}
