#![no_std]
#![no_main]
#![deny(clippy::large_stack_frames)]

mod animation;

use crate::animation::Animation;
use defmt::{info, unwrap, warn};
use embassy_executor::Spawner;
use embassy_futures::select::Either3;
use embassy_futures::select::select3;
use embassy_net::{Stack, StackResources};
use embassy_stm32::eth::{Ethernet, GenericPhy, PacketQueue, Sma};
use embassy_stm32::gpio::{Input, OutputType, Pull};
use embassy_stm32::peripherals::{ETH, ETH_SMA, TIM3, TIM4, TIM12};
use embassy_stm32::rng::Rng;
use embassy_stm32::time::Hertz;
use embassy_stm32::timer::GeneralInstance4Channel;
use embassy_stm32::timer::low_level::CountingMode;
use embassy_stm32::timer::simple_pwm::{PwmPin, SimplePwm, SimplePwmChannel};
use embassy_stm32::{Config, bind_interrupts, eth, peripherals, rng};
use embassy_time::{Duration, Instant, Ticker, Timer};
use sacn::embassy::{
    Receiver as SacnReceiver, ReceiverResources, Source as SacnSource, SourceResources,
};
use sacn::{Cid, ReceiverConfig, ReceiverEventRef, SourceConfig, Universe, UniverseConfig};
use static_cell::ConstStaticCell;
use uuid::{Uuid, uuid};

use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    ETH => eth::InterruptHandler;
    HASH_RNG => rng::InterruptHandler<peripherals::RNG>;
});

const MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x38, 0xd8, 0xa5];

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
        config.rcc.hse = Some(Hse {
            freq: Hertz(8_000_000),
            mode: HseMode::Bypass,
        });
        config.rcc.pll_src = PllSource::HSE;
        config.rcc.pll = Some(Pll {
            prediv: PllPreDiv::DIV4,
            mul: PllMul::MUL180,
            divp: Some(PllPDiv::DIV2), // 8mhz / 4 * 180 / 2 = 180Mhz.
            divq: None,
            divr: None,
        });
        config.rcc.ahb_pre = AHBPrescaler::DIV1;
        config.rcc.apb1_pre = APBPrescaler::DIV4;
        config.rcc.apb2_pre = APBPrescaler::DIV2;
        config.rcc.sys = Sysclk::PLL1_P;
    }
    let p = embassy_stm32::init(config);

    info!("Hello World!");

    let mut rng = Rng::new(p.RNG, Irqs);
    let mut seed = [0; 8];
    let _ = rng.async_fill_bytes(&mut seed).await;
    let seed = u64::from_le_bytes(seed);

    static PACKETS: ConstStaticCell<PacketQueue<4, 4>> =
        ConstStaticCell::new(PacketQueue::<4, 4>::new());
    let eth = Ethernet::new(
        PACKETS.take(),
        p.ETH,
        Irqs,
        p.PA1,
        p.PA7,
        p.PC4,
        p.PC5,
        p.PG13,
        p.PB13,
        p.PG11,
        MAC,
        p.ETH_SMA,
        p.PA2,
        p.PC1,
    );

    let net_config = embassy_net::Config::dhcpv4(Default::default());
    // Uncomment/swap for static IP configuration
    // let net_config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
    //     address: embassy_net::Ipv4Cidr::new(core::net::Ipv4Addr::new(10, 0, 0, 39), 24),
    //     gateway: None,
    //     dns_servers: Default::default(),
    // });

    static STACK_RESOURCES: ConstStaticCell<StackResources<3>> =
        ConstStaticCell::new(StackResources::new());
    let (stack, runner) = embassy_net::new(eth, net_config, STACK_RESOURCES.take(), seed);

    // The user button (B1). On this Nucleo board it reads high while pressed.
    let button = Input::new(p.PC13, Pull::None);

    // 3 user LEDs on the board driven by PWM
    const LED_PWM_FREQ: Hertz = Hertz(1_000);
    let ld1 = SimplePwm::new(
        p.TIM3,
        None,
        None,
        Some(PwmPin::new(p.PB0, OutputType::PushPull)),
        None,
        LED_PWM_FREQ,
        CountingMode::EdgeAlignedUp,
    )
    .split()
    .ch3;
    let ld2 = SimplePwm::new(
        p.TIM4,
        None,
        Some(PwmPin::new(p.PB7, OutputType::PushPull)),
        None,
        None,
        LED_PWM_FREQ,
        CountingMode::EdgeAlignedUp,
    )
    .split()
    .ch2;
    let ld3 = SimplePwm::new(
        p.TIM12,
        Some(PwmPin::new(p.PB14, OutputType::PushPull)),
        None,
        None,
        None,
        LED_PWM_FREQ,
        CountingMode::EdgeAlignedUp,
    )
    .split()
    .ch1;
    let leds = Leds::new(ld1, ld2, ld3);

    spawner.spawn(unwrap!(net_task(runner)));

    // Wait for DHCP if necessary
    info!("waiting for network stack to be ready...");
    stack.wait_config_up().await;
    if let Some(cfg) = stack.config_v4() {
        info!("network up, address: {}", cfg.address);
    }

    spawner.spawn(unwrap!(sacn_task(stack, button, leds)));

    // The sACN task owns all the interesting work from here on.
    // This task can safely exit.
}

type Device = Ethernet<'static, ETH, GenericPhy<Sma<'static, ETH_SMA>>>;

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Device>) -> ! {
    runner.run().await
}

const FRAME_INTERVAL: Duration = Duration::from_millis(30);

/// How long the user button must be held to count as a long press.
const LONG_PRESS: Duration = Duration::from_secs(1);

const LISTEN_UNIVERSE: u16 = 1;
const MIRROR_UNIVERSE: u16 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum OperatingMode {
    /// Receive universe 1, re-source the merged result on universe 2, and show
    /// the first three received slots on the LEDs.
    Mirror,
    /// Source the animated patterns on universe 1 and show their first three
    /// slots on the LEDs. Short presses cycle the pattern.
    Pattern,
}

impl OperatingMode {
    fn toggled(self) -> Self {
        match self {
            OperatingMode::Mirror => OperatingMode::Pattern,
            OperatingMode::Pattern => OperatingMode::Mirror,
        }
    }
}

#[embassy_executor::task]
async fn sacn_task(stack: Stack<'static>, button: Input<'static>, mut leds: Leds) {
    sacn::embassy_static_storage! {
        struct Caps {
            rx_universes: 1,
            rx_sources_per_universe: 4,
            rx_sync_addresses: 0,
            // TODO: only one universe should be required here (#17)
            tx_universes: 2,
            tx_unicast_per_universe: 0,
            det_sources: 0,
            det_universes_per_source: 0,
        }
    }

    static SOURCE_RES: ConstStaticCell<SourceResources<Caps>> =
        ConstStaticCell::new(Caps::embassy_source_resources());
    static RX_RES: ConstStaticCell<ReceiverResources<Caps>> =
        ConstStaticCell::new(Caps::embassy_receiver_resources());

    let cid = get_cid();
    let mut source = SacnSource::new(
        stack,
        SOURCE_RES.take(),
        SourceConfig::new(cid, "STM32 sACN Example"),
    )
    .expect("failed to bind sACN source");

    let listen_universe = Universe::new(LISTEN_UNIVERSE).expect("universe 1 is valid");
    let mirror_universe = Universe::new(MIRROR_UNIVERSE).expect("universe 2 is valid");

    let mut receiver = SacnReceiver::bind(stack, RX_RES.take(), ReceiverConfig::new())
        .expect("failed to bind sACN receiver");
    receiver
        .listen(listen_universe)
        .expect("joining universe 1's multicast group");

    // Boot into mirror mode: source the merged universe-1 data on universe 2.
    let mut mode = OperatingMode::Mirror;
    source
        .add_universe(UniverseConfig::new(mirror_universe))
        .expect("capacity for 1 universe is assured");

    info!(
        "mirror mode: receiving universe {}, sourcing merged data on universe {}",
        LISTEN_UNIVERSE, MIRROR_UNIVERSE
    );

    static LEVELS: ConstStaticCell<[u8; 512]> = ConstStaticCell::new([0u8; 512]);
    let levels = LEVELS.take();
    let mut animation = Animation::new();

    // Button edge/hold tracking, sampled on every frame tick.
    let mut press_started: Option<Instant> = None;
    let mut long_fired = false;

    let mut ticker = Ticker::every(FRAME_INTERVAL);

    loop {
        // Race source transmission, received events, and the frame tick.
        match select3(source.run(), receiver.next_event(), ticker.next()).await {
            Either3::First(result) => {
                // `run` only returns on error (Ok cannot be constructed)
                let Err(error) = result;
                warn!("sACN source error: {:?}", error);
                Timer::after(Duration::from_millis(100)).await;
            }
            Either3::Second(event) => {
                let Some(event) = event else { continue };
                if mode == OperatingMode::Mirror {
                    if let ReceiverEventRef::MergedData(merged) = &event {
                        // Re-source the merged result on universe 2 and show its
                        // first three slots on the LEDs.
                        let merged_levels = merged.levels();
                        source.update_levels(mirror_universe, merged_levels);
                        leds.show(merged_levels);
                    } else {
                        log_receiver_event(&event);
                    }
                }
            }
            Either3::Third(()) => {
                // Frame tick: sample the button, then (in pattern mode) advance
                // and retransmit the animation.
                let pressed = button.is_high();
                let now = Instant::now();
                match (press_started, pressed) {
                    (None, true) => {
                        // Rising edge: start timing this press.
                        press_started = Some(now);
                        long_fired = false;
                    }
                    (Some(start), true) => {
                        // Held: switch modes once the long-press threshold is
                        // crossed, and only once per press.
                        if !long_fired && now.duration_since(start) >= LONG_PRESS {
                            long_fired = true;
                            mode = mode.toggled();
                            switch_mode(
                                &mut source,
                                &mut leds,
                                mode,
                                listen_universe,
                                mirror_universe,
                            );
                        }
                    }
                    (Some(_), false) => {
                        // Falling edge: a release that never became a long press
                        // is a short press, which cycles the pattern.
                        if !long_fired && mode == OperatingMode::Pattern {
                            animation.next_pattern();
                            info!("short press: pattern -> {}", animation.pattern.name());
                        }
                        press_started = None;
                    }
                    (None, false) => {}
                }

                if mode == OperatingMode::Pattern {
                    animation.advance();
                    animation.render(levels);
                    source.update_levels(listen_universe, levels);
                    leds.show(levels);
                }
            }
        }
    }
}

/// Applies a mode switch to the source's universes and the LEDs: the outgoing
/// universe is terminated gracefully and the incoming one is added.
fn switch_mode(
    source: &mut SacnSource<'_, impl sacn::embassy::SourceStorage>,
    leds: &mut Leds,
    mode: OperatingMode,
    listen_universe: Universe,
    mirror_universe: Universe,
) {
    match mode {
        OperatingMode::Pattern => {
            source.remove_universe(mirror_universe);
            source
                .add_universe(UniverseConfig::new(listen_universe))
                .expect("couldn't add universe");
            info!(
                "long press: pattern source mode on universe {}",
                listen_universe.get()
            );
        }
        OperatingMode::Mirror => {
            source.remove_universe(listen_universe);
            source
                .add_universe(UniverseConfig::new(mirror_universe))
                .expect("couldn't add universe");
            // No data until the next merged frame arrives; start dark.
            leds.off();
            info!(
                "long press: mirror mode (universe {} -> {})",
                listen_universe.get(),
                mirror_universe.get()
            );
        }
    }
}

/// The three onboard user LEDs, each dimmed via PWM to show a DMX slot level
/// from off (0) to fully on (255): LD1 (PB0), LD2 (PB7), LD3 (PB14).
struct Leds {
    ld1: SimplePwmChannel<'static, TIM3>,
    ld2: SimplePwmChannel<'static, TIM4>,
    ld3: SimplePwmChannel<'static, TIM12>,
}

impl Leds {
    fn new(
        mut ld1: SimplePwmChannel<'static, TIM3>,
        mut ld2: SimplePwmChannel<'static, TIM4>,
        mut ld3: SimplePwmChannel<'static, TIM12>,
    ) -> Self {
        ld1.enable();
        ld2.enable();
        ld3.enable();
        let mut leds = Self { ld1, ld2, ld3 };
        leds.off();
        leds
    }

    /// Sets each LED's brightness from the first three slots of `levels`
    /// (missing slots read as 0).
    fn show(&mut self, levels: &[u8]) {
        set_level(&mut self.ld1, levels.first().copied().unwrap_or(0));
        set_level(&mut self.ld2, levels.get(1).copied().unwrap_or(0));
        set_level(&mut self.ld3, levels.get(2).copied().unwrap_or(0));
    }

    /// Turns every LED off.
    fn off(&mut self) {
        self.ld1.set_duty_cycle_fully_off();
        self.ld2.set_duty_cycle_fully_off();
        self.ld3.set_duty_cycle_fully_off();
    }
}

/// Sets a PWM channel's duty cycle from a DMX level, mapping 0..=255 linearly to
/// off..=full brightness.
fn set_level<T: GeneralInstance4Channel>(channel: &mut SimplePwmChannel<'_, T>, level: u8) {
    channel.set_duty_cycle_fraction(u32::from(level), u32::from(u8::MAX));
}

/// Logs a received merging-receiver event over `defmt`.
fn log_receiver_event(event: &ReceiverEventRef<'_, impl sacn::embassy::ReceiverStorage>) {
    match event {
        ReceiverEventRef::MergedData(merged) => {
            let levels = merged.levels();
            info!(
                "merged data on universe {}: first slots {:?}",
                merged.universe.get(),
                &levels[..levels.len().min(8)]
            );
        }
        ReceiverEventRef::SamplingStarted { universe } => {
            info!("sampling started on universe {}", universe.get());
        }
        ReceiverEventRef::SamplingEnded { universe } => {
            info!("sampling ended on universe {}", universe.get());
        }
        ReceiverEventRef::SourcesLost { universe, sources } => {
            for source in *sources {
                info!(
                    "source '{}' lost on universe {}",
                    source.name.as_str(),
                    universe.get()
                );
            }
        }
        _ => {}
    }
}

fn get_cid() -> Cid {
    // A device's CID must be permanently associated with a specific device,
    // and must be stable across the lifetime of the device. The typical way
    // to accomplish this is by generating a V5 UUID, with a namespace (which
    // is just a UUID you generate once) meaning "all products of this type",
    // and some input value that is per-device and stable.
    //
    // Here we use the STM32's factory-programmed 96-bit unique device ID as
    // the stable value.

    // Generated from uuidgenerator.net on 2026-07-12
    const CID_NAMESPACE: Uuid = uuid!("62974845-9d5e-4f9b-91dc-0740477fe8ab");

    let uid = embassy_stm32::uid::uid();
    uuid::Uuid::new_v5(&CID_NAMESPACE, &uid).into()
}
