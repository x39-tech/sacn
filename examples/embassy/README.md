# Embassy Example

This example app for the `sacn` library is designed for the [STM32F439](https://www.st.com/en/microcontrollers-microprocessors/stm32f429-439.html) family, an ARM Cortex-M4 microcontroller with onboard Ethernet MAC. The example specifically targets the [NUCLEO-F439ZI](https://www.st.com/en/evaluation-tools/nucleo-f439zi.html) development board, which is available from several retailers for about $35.

This application should be easily portable to any hardware platform that has some type of network interface and good Embassy support.

## Setup

To flash this application to your NUCLEO-F439ZI development board and run it:

1. Install [probe-rs](https://probe.rs/), if it is not installed already.
2. Connect your NUCLEO-F439ZI development board to your host machine using a micro-USB cable connected to the USB power port CN1.
3. Connect the Ethernet port on the NUCLEO-F439ZI to wherever you want to observe the sACN functionality (a direct connection to your host machine will work).
4. Modify the firmware as necessary for your network setup. By default, it is configured to wait for DHCP configuration before beginning sACN. If you would prefer to assign a static IP, check the commented alternative code in the `main` function.
5. Run `cargo build --release` in this directory to build the firmware.
6. Run `cargo run --release` in this directory to flash the firmware to the device with `probe-rs`, and begin running.

You will see logs in the `probe-rs` console via `defmt`, and the device will begin sending sACN on universe 1. Cycle through different patterns of sACN being sent by pressing the user button B1.

If you're looking for a tool to verify the sACN output, [sACNView](https://sacnview.org) is the go-to choice.

## Seeing Flash/RAM usage

The root `sacn` repo has some `cargo xtask` aliases to produce various resource usage summaries for this application. These commands must be run at the root of the repo, not in this directory.

- `cargo xtask embassy-size` prints a summary of onboard flash and SRAM usage as well as a breakdown of each ELF section.
- `cargo xtask embassy-bloat` breaks down flash usage by crate and symbol, listing the largest offenders.
- `cargo xtask embassy-ram` does the same for static RAM usage.

## Troubleshooting

### 'Failed to open the debug probe'

On Windows, you might see a message like this when running `cargo run`:

```
Error: Failed to open probe: Failed to open the debug probe.

Caused by:
    0: The debug probe could not be created.
    1: A USB error occurred.
    2: could not determine driver for interface
```

Install the USB drivers provided by the [STSW-LINK009](https://www.st.com/en/development-tools/stsw-link009.html) software package on Windows, then try again.
