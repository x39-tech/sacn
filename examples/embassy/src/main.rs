#![no_std]
#![no_main]
#![deny(clippy::large_stack_frames)]

mod animation;

use crate::animation::Animation;
use defmt::{info, unwrap, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::{Stack, StackResources};
use embassy_stm32::eth::{Ethernet, GenericPhy, PacketQueue, Sma};
use embassy_stm32::gpio::{Input, Level, Output, Pull, Speed};
use embassy_stm32::peripherals::{ETH, ETH_SMA};
use embassy_stm32::rng::Rng;
use embassy_stm32::time::Hertz;
use embassy_stm32::{Config, bind_interrupts, eth, peripherals, rng};
use embassy_time::{Duration, Ticker, Timer};
use sacn::embassy::{Source as SacnSource, SourceResources};
use sacn::{Cid, SourceConfig, Universe, UniverseConfig};
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

    let mut led = Output::new(p.PB0, Level::High, Speed::Low);
    let button = Input::new(p.PC13, Pull::None);

    spawner.spawn(unwrap!(net_task(runner)));

    // Wait for DHCP if necessary
    info!("waiting for network stack to be ready...");
    stack.wait_config_up().await;
    if let Some(cfg) = stack.config_v4() {
        info!("network up, address: {}", cfg.address);
    }

    spawner.spawn(unwrap!(sacn_task(stack, button)));

    loop {
        led.set_high();
        Timer::after_secs(1).await;

        led.set_low();
        Timer::after_secs(1).await;
    }
}

type Device = Ethernet<'static, ETH, GenericPhy<Sma<'static, ETH_SMA>>>;

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Device>) -> ! {
    runner.run().await
}

const FRAME_INTERVAL: Duration = Duration::from_millis(30);

#[embassy_executor::task]
async fn sacn_task(stack: Stack<'static>, button: Input<'static>) {
    sacn::embassy_static_storage! {
        struct Caps {
            tx_universes: 1,
            tx_unicast_per_universe: 0
        }
    }

    static SOURCE_RES: ConstStaticCell<SourceResources<Caps>> =
        ConstStaticCell::new(Caps::embassy_source_resources());

    let cid = get_cid();
    let mut source = SacnSource::new(
        stack,
        SOURCE_RES.take(),
        SourceConfig::new(cid, "STM32 sACN Example"),
    )
    .expect("failed to bind sACN source");

    let universe = Universe::new(1).expect("universe 1 is valid");
    source
        .add_universe(UniverseConfig::new(universe))
        .expect("capacity for 1 universe is assured");

    info!("transmitting sACN on universe 1");

    static LEVELS: ConstStaticCell<[u8; 512]> = ConstStaticCell::new([0u8; 512]);

    let mut animation = Animation::new();
    let levels = LEVELS.take();
    let mut button_was_high = false;

    animation.render(levels);
    source.update_levels(universe, levels);

    let mut ticker = Ticker::every(FRAME_INTERVAL);

    loop {
        match select(source.run(), ticker.next()).await {
            Either::First(result) => {
                // `run` only returns on error (Ok cannot be constructed)
                let Err(error) = result;
                warn!("sACN source error: {:?}", error);
                Timer::after(Duration::from_millis(100)).await;
            }
            Either::Second(()) => {
                // Frame tick: sample the button (falling edge = press), then
                // advance and retransmit the animation.

                let high = button.is_high();
                if high && !button_was_high {
                    animation.next_pattern();
                    info!("button pressed: pattern -> {}", animation.pattern.name());
                }
                button_was_high = high;

                animation.advance();
                animation.render(levels);
                source.update_levels(universe, levels);
            }
        }
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
