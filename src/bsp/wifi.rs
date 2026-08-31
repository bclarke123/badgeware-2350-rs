//! WiFi via the on-board RM2 (CYW43439) radio: joins a network and hands
//! back a ready [`embassy_net::Stack`] plus the cyw43 [`Control`] handle.
//!
//! Both boards wire the radio like a Pico W — WL_ON GPIO23, DATA GPIO24,
//! CS GPIO25, CLK GPIO29 — driven here over PIO SPI (PIO0 SM0) with the
//! RM2 clock divider. Radio firmware is vendored in `firmware/` (see its
//! README and permissive binary license) and baked in with
//! `cyw43::aligned_bytes!`.

use cyw43::{Control, JoinOptions, NetDriver, SpiBus, State};
#[cfg(feature = "badger2040w")]
use cyw43_pio::DEFAULT_CLOCK_DIVIDER as RM2_CLOCK_DIVIDER;
#[cfg(not(feature = "badger2040w"))]
use cyw43_pio::RM2_CLOCK_DIVIDER;
use cyw43_pio::PioSpi;
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_rp::clocks::RoscRng;
use embassy_rp::dma;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::interrupt::typelevel::Binding;
use embassy_rp::peripherals::{PIN_23, PIN_24, PIN_25, PIN_29, PIO0};
use embassy_rp::pio::{self, Pio};
use embassy_rp::Peri;
use embassy_time::{with_timeout, Duration, Timer};
use static_cell::StaticCell;

/// A joined network: the socket stack and the radio control handle.
pub struct Wifi {
    pub stack: Stack<'static>,
    pub control: Control<'static>,
}

static STATE: StaticCell<State> = StaticCell::new();
static RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, NetDriver<'static>>) -> ! {
    runner.run().await
}

/// Brings the radio up, joins `ssid`, and waits for a DHCP lease. One-shot:
/// the driver tasks hold the `'static` state, so call it once per boot.
#[expect(clippy::too_many_arguments, reason = "fixed board wiring, called once from main")]
pub async fn connect(
    spawner: Spawner,
    pio: Peri<'static, PIO0>,
    irqs: impl Binding<<PIO0 as pio::Instance>::Interrupt, pio::InterruptHandler<PIO0>>,
    dma: dma::Channel<'static>,
    pwr: Peri<'static, PIN_23>,
    dio: Peri<'static, PIN_24>,
    cs: Peri<'static, PIN_25>,
    clk: Peri<'static, PIN_29>,
    ssid: &str,
    passphrase: &str,
) -> Result<Wifi, cyw43::JoinError> {
    let fw = cyw43::aligned_bytes!("../../firmware/43439A0.bin");
    let clm = cyw43::aligned_bytes!("../../firmware/43439A0_clm.bin");
    let nvram = cyw43::aligned_bytes!("../../firmware/nvram_rp2040.bin");

    let pwr = Output::new(pwr, Level::Low);
    let cs = Output::new(cs, Level::High);
    let mut pio = Pio::new(pio, irqs);
    let spi = PioSpi::new(&mut pio.common, pio.sm0, RM2_CLOCK_DIVIDER, pio.irq0, cs, dio, clk, dma);

    let state = STATE.init(State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;
    spawner.spawn(cyw43_task(runner).unwrap());
    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let seed = RoscRng.next_u64();
    let (stack, runner) = embassy_net::new(
        net_device,
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.spawn(net_task(runner).unwrap());

    // Join with bounded retries: a join can fail transiently (weak signal,
    // AP busy) and a DHCP lease can stall, so both get timeouts and another
    // go rather than hanging the boot forever.
    let mut last_err = cyw43::JoinError::JoinFailure(0);
    for attempt in 1..=5 {
        log::info!("join '{}' (attempt {}/5)", ssid, attempt);
        match with_timeout(
            Duration::from_secs(30),
            control.join(ssid, JoinOptions::new(passphrase.as_bytes())),
        )
        .await
        {
            Ok(Ok(())) => {
                log::info!("joined; waiting for DHCP");
                if with_timeout(Duration::from_secs(20), stack.wait_config_up()).await.is_ok() {
                    if let Some(cfg) = stack.config_v4() {
                        log::info!("up: {}", cfg.address);
                    }
                    return Ok(Wifi { stack, control });
                }
                log::warn!("no DHCP lease in 20 s; rejoining");
                let _ = control.leave().await;
            }
            Ok(Err(e)) => {
                log::warn!("join failed: {:?}", e);
                last_err = e;
            }
            Err(_) => {
                log::warn!("join attempt timed out");
                let _ = control.leave().await;
            }
        }
        Timer::after(Duration::from_secs(2)).await;
    }
    Err(last_err)
}

/// Brings the radio up as a (closed) access point host: the driver and a
/// static-IP stack (192.168.4.1/24) are ready, but no AP is broadcast until
/// [`Wifi::ap_on`]. For provisioning flows (captive-portal style setup).
#[expect(clippy::too_many_arguments, reason = "fixed board wiring, called once from main")]
pub async fn access_point(
    spawner: Spawner,
    pio: Peri<'static, PIO0>,
    irqs: impl Binding<<PIO0 as pio::Instance>::Interrupt, pio::InterruptHandler<PIO0>>,
    dma: dma::Channel<'static>,
    pwr: Peri<'static, PIN_23>,
    dio: Peri<'static, PIN_24>,
    cs: Peri<'static, PIN_25>,
    clk: Peri<'static, PIN_29>,
) -> Wifi {
    let fw = cyw43::aligned_bytes!("../../firmware/43439A0.bin");
    let clm = cyw43::aligned_bytes!("../../firmware/43439A0_clm.bin");
    let nvram = cyw43::aligned_bytes!("../../firmware/nvram_rp2040.bin");

    let pwr = Output::new(pwr, Level::Low);
    let cs = Output::new(cs, Level::High);
    let mut pio = Pio::new(pio, irqs);
    let spi = PioSpi::new(&mut pio.common, pio.sm0, RM2_CLOCK_DIVIDER, pio.irq0, cs, dio, clk, dma);

    let state = STATE.init(State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;
    spawner.spawn(cyw43_task(runner).unwrap());
    control.init(clm).await;

    let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(AP_ADDR, 24),
        gateway: None,
        dns_servers: heapless::Vec::new(),
    });
    let seed = RoscRng.next_u64();
    let (stack, runner) = embassy_net::new(net_device, config, RESOURCES.init(StackResources::new()), seed);
    spawner.spawn(net_task(runner).unwrap());
    Wifi { stack, control }
}

/// The address the badge uses when hosting an access point.
pub const AP_ADDR: embassy_net::Ipv4Address = embassy_net::Ipv4Address::new(192, 168, 4, 1);

impl Wifi {
    /// Starts broadcasting a WPA2 access point (after [`access_point`]).
    pub async fn ap_on(&mut self, ssid: &str, passphrase: &str, channel: u8) {
        self.control.start_ap_wpa2(ssid, passphrase, channel).await;
        log::info!("ap '{}' up on channel {}", ssid, channel);
    }

    /// Stops broadcasting the access point.
    pub async fn ap_off(&mut self) {
        self.control.close_ap().await;
        log::info!("ap down");
    }

    /// Disassociates from the network (the stack and tasks stay alive); use
    /// [`Wifi::rejoin`] to come back. For apps that idle between bursts of
    /// traffic and want the radio quiet in between.
    pub async fn leave(&mut self) {
        self.control.leave().await;
        log::info!("wifi: left network");
    }

    /// Rejoins after [`Wifi::leave`] and waits (bounded) for connectivity.
    pub async fn rejoin(&mut self, ssid: &str, passphrase: &str) -> bool {
        for attempt in 1..=3 {
            log::info!("rejoin '{}' (attempt {}/3)", ssid, attempt);
            match with_timeout(
                Duration::from_secs(30),
                self.control.join(ssid, JoinOptions::new(passphrase.as_bytes())),
            )
            .await
            {
                Ok(Ok(())) => {
                    if with_timeout(Duration::from_secs(20), self.stack.wait_config_up()).await.is_ok() {
                        return true;
                    }
                    let _ = self.control.leave().await;
                }
                Ok(Err(e)) => log::warn!("rejoin failed: {:?}", e),
                Err(_) => {
                    log::warn!("rejoin timed out");
                    let _ = self.control.leave().await;
                }
            }
            Timer::after(Duration::from_secs(2)).await;
        }
        false
    }
}

// Rust guideline compliant 2026-08-30
