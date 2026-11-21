# Rusty Probe

This firmware implements an CMSIS-DAP v1 and v2 compatible probe.

## Building

You can build the project and generate a `.uf2` file as follows:

```console
# Install elf2uf2-rs and flip-link (you only need to do this once)
cargo install elf2uf2-rs flip-link

# Build the ELF without logging
DEFMT_LOG=off cargo build --release --bin app

# Generate .uf2 file
elf2uf2-rs target/thumbv6m-none-eabi/release/app app
```

Start the RP2040 in bootloader mode and drop the `app.uf2` file to it, done! 

## Virtual COM port

The probe bridges a USB serial port (CDC-ACM) to the target UART on the `T_VCP_TX` and `T_VCP_RX` pins. Baud rate, data bits, parity and stop bits follow what the host configures (at least 300 baud, 1.5 stop bits become 2, mark and space parity are not supported). Data received from the target while no terminal has the port open (DTR low) is dropped.

On Linux the port shows up as `/dev/ttyACM*`, on Windows as a COM port using the built-in driver, and the CMSIS-DAP v2 interface binds to WinUSB automatically.

## Running with `defmt` logs without debugger

If you don't have a debugger/programmer to program or debug your rusty-probe with, you can still run the probe and see the defmt logs it prints.

To do so, start the probe in the USB bootloader by powering it on while holding the button pressed. Mount the resulting block device.

Then, perform the following steps:

```console
# Install elf2uf2-rs, flip-link, and defmt-print (you only need to do this once)
cargo install elf2uf2-rs flip-link defmt-print

# Build the binary with the desired level of logging, and run
# it using `xtask`
XTASK_SERIAL=/dev/ttyACM0 DEFMT_LOG=debug cargo rrb-usb app
```

You can now repeat the above command, which will automatically restart and flash your Rusty Probe. Note that this does require that the block device is mounted each time.

The logs and the reboot command use their own serial port, the first one the probe enumerates (`/dev/ttyACM0` if no other `ttyACM` devices are connected). The virtual COM port stays available as the second one.

You may have to change `XTASK_SERIAL`, and on non-x86 linux platforms, you must update the runner configuration with the correct target (see [`.cargo/config.toml`](.cargo/config.toml#L15)).

## Feature TODO list

- [ ] Add support for JTAG (only SWD works right now)
- [ ] Add support for SWO
- [x] Add support for VCP

