#![no_std]
#![no_main]

mod game;
mod gfx;

use crate::game::{Action, Button, Direction, Game, Screen};
use core::cell::RefCell;
use critical_section::Mutex;
use defmt_rtt as _;
use embedded_graphics::{
    image::Image,
    prelude::*,
    text::{Baseline, Text},
};
use embedded_hal::digital::v2::InputPin;
use fugit::RateExtU32;
use panic_halt as _;
use sh1106::{prelude::*, Builder};
use usb_device::class_prelude::UsbBusAllocator;
use usb_device::device::{UsbDeviceBuilder, UsbVidPid};
use usbd_serial::SerialPort;
use usbd_serial::USB_CLASS_CDC;
use waveshare_rp2040_zero::entry;
use waveshare_rp2040_zero::{
    hal::{
        clocks::{init_clocks_and_plls, Clock},
        gpio::{self, Interrupt},
        i2c::I2C,
        pac,
        pac::interrupt,
        timer::Timer,
        usb::UsbBus,
        watchdog::Watchdog,
        Sio,
    },
    XOSC_CRYSTAL_FREQ,
};

/*
const FRAMES: &[ImageRaw<BinaryColor>] = &[
    ImageRaw::new(include_bytes!("../data/frame1.raw"), 128),
    ImageRaw::new(include_bytes!("../data/frame2.raw"), 128),
    ImageRaw::new(include_bytes!("../data/frame3.raw"), 128),
    ImageRaw::new(include_bytes!("../data/frame4.raw"), 128),
];
*/

type ButtonPin1 = gpio::Pin<gpio::bank0::Gpio10, gpio::FunctionSioInput, gpio::PullUp>;
type ButtonPin2 = gpio::Pin<gpio::bank0::Gpio11, gpio::FunctionSioInput, gpio::PullUp>;
type ButtonPin3 = gpio::Pin<gpio::bank0::Gpio12, gpio::FunctionSioInput, gpio::PullUp>;
type ButtonPin4 = gpio::Pin<gpio::bank0::Gpio7, gpio::FunctionSioInput, gpio::PullUp>;
type LedAndButton = (ButtonPin1, ButtonPin2, ButtonPin3, ButtonPin4);

static GLOBAL_PINS: Mutex<RefCell<Option<LedAndButton>>> = Mutex::new(RefCell::new(None));
static EVENTS: Mutex<RefCell<[u8; 3]>> = Mutex::new(RefCell::new([0x31; 3]));
static ACTION: Mutex<RefCell<Option<Action>>> = Mutex::new(RefCell::new(None));

#[entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();

    // Configure clocks and timers
    let mut watchdog = Watchdog::new(pac.WATCHDOG);
    let clocks = init_clocks_and_plls(
        XOSC_CRYSTAL_FREQ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let timer = Timer::new(pac.TIMER, &mut pac.RESETS, &clocks);
    // let mut delay = timer.count_down();

    // The single-cycle I/O block controls our GPIO pins
    let sio = Sio::new(pac.SIO);

    // Set the pins to their default state
    let pins = gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // Configure display
    let i2c = I2C::i2c0(
        pac.I2C0,
        pins.gpio4.into_function(), // sda
        pins.gpio5.into_function(), // scl
        400.kHz(),
        &mut pac.RESETS,
        clocks.peripheral_clock.freq(),
    );
    let mut display: GraphicsMode<_> = Builder::new()
        .with_rotation(DisplayRotation::Rotate180)
        .connect_i2c(i2c)
        .into();
    display.init().unwrap();

    // Set up the GPIO pin that will be our input
    let button1 = pins.gpio10.reconfigure();
    let button2 = pins.gpio11.reconfigure();
    let button3 = pins.gpio12.reconfigure();
    let button4 = pins.gpio7.reconfigure();

    // Trigger on the 'falling edge' of the input pin.
    // This will happen as the button is being pressed
    button1.set_interrupt_enabled(Interrupt::EdgeHigh, true);
    button1.set_interrupt_enabled(Interrupt::EdgeLow, true);
    button2.set_interrupt_enabled(Interrupt::EdgeHigh, true);
    button2.set_interrupt_enabled(Interrupt::EdgeLow, true);
    button3.set_interrupt_enabled(Interrupt::EdgeHigh, true);
    button3.set_interrupt_enabled(Interrupt::EdgeLow, true);
    button4.set_interrupt_enabled(Interrupt::EdgeHigh, true);
    button4.set_interrupt_enabled(Interrupt::EdgeLow, true);

    // Give away our pins by moving them into the `GLOBAL_PINS` variable.
    // We won't need to access them in the main thread again
    critical_section::with(|cs| {
        GLOBAL_PINS
            .borrow(cs)
            .replace(Some((button1, button2, button3, button4)));
    });

    // Configure USB serial
    let usb_bus = UsbBusAllocator::new(UsbBus::new(
        pac.USBCTRL_REGS,
        pac.USBCTRL_DPRAM,
        clocks.usb_clock,
        true,
        &mut pac.RESETS,
    ));
    let mut serial = SerialPort::new(&usb_bus);
    let mut usb_dev = UsbDeviceBuilder::new(&usb_bus, UsbVidPid(0x16c0, 0x27dd))
        .product("Serial port")
        .device_class(USB_CLASS_CDC)
        .build();

    // Unmask the IO_BANK0 IRQ so that the NVIC interrupt controller
    // will jump to the interrupt function when the interrupt occurs.
    // We do this last so that the interrupt can't go off while
    // it is in the middle of being configured
    unsafe {
        pac::NVIC::unmask(pac::Interrupt::IO_IRQ_BANK0);
    }

    let mut last_action: Option<Action> = None;
    let mut game = Game::new();

    let mut last_tick = timer.get_counter();

    loop {
        let mut buf = [0u8; 3];
        critical_section::with(|cs| {
            let events = EVENTS.borrow(cs);
            buf.copy_from_slice(&*events.borrow());

            if let Some(action) = ACTION.borrow(cs).take() {
                game.action(&action);
                last_action = Some(action);
            }
        });

        let elapsed = last_tick.duration_since_epoch();
        if elapsed > game::TICK_INTERVAL {
            game.tick();
            last_tick = timer.get_counter();
        }

        // draw image
        display.clear();
        match game.screen() {
            Screen::Start => {
                let im = Image::new(&gfx::FERRIS_REVOLVER, Point::new(0, game::START_Y as i32));
                im.draw(&mut display).unwrap();

                Text::with_baseline(
                    "Press shoot to start",
                    Point::new(25, 55),
                    gfx::TEXT_STYLE,
                    Baseline::Top,
                )
                .draw(&mut display)
                .unwrap();
            }
            Screen::Normal => {
                // show ferris
                let im = Image::new(&gfx::FERRIS_REVOLVER, Point::new(0, game.y() as i32));
                im.draw(&mut display).unwrap();

                // score
                let mut score = itoa::Buffer::new();
                let score = score.format(game.score());
                Text::with_baseline(
                    score,
                    Point::new(gfx::text_align_right(score, gfx::SCREEN_WIDTH), 0),
                    gfx::TEXT_STYLE,
                    Baseline::Top,
                )
                .draw(&mut display)
                .unwrap();
            }
            Screen::Reload => {
                Text::with_baseline("Reload", Point::new(5, 5), gfx::TEXT_STYLE, Baseline::Top)
                    .draw(&mut display)
                    .unwrap();
            }
        }
        display.flush().unwrap();

        // test stuff
        serial.write(&buf).ok();
        /*
        serial
            .write(match last_action {
                Some(Action::Rotate(Direction::Clockwise)) => b" rt cw",
                Some(Action::Rotate(Direction::CounterClock)) => b" rt cc",
                Some(Action::ReloadToggle) => b" rl",
                Some(Action::Shoot) => b" shoot",
                None => b" - none",
            })
            .ok();
        */
        serial.write(b"\n").ok();

        if usb_dev.poll(&mut [&mut serial]) {
            let mut buf = [0u8; 64];
            serial.read(&mut buf[..]).ok();
        }
    }

    /*
    let mut pac = pac::Peripherals::take().unwrap();

    // Configure clocks and timers
    let mut watchdog = Watchdog::new(pac.WATCHDOG);
    let clocks = init_clocks_and_plls(
        XOSC_CRYSTAL_FREQ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let timer = Timer::new(pac.TIMER, &mut pac.RESETS, &clocks);
    let mut delay = timer.count_down();

    // Configure gpio
    let sio = Sio::new(pac.SIO);
    let pins = Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // Configure display
    let i2c = I2C::i2c0(
        pac.I2C0,
        pins.gp4.into_function(), // sda
        pins.gp5.into_function(), // scl
        400.kHz(),
        &mut pac.RESETS,
        clocks.peripheral_clock.freq(),
    );
    let mut display: GraphicsMode<_> = Builder::new()
        .with_rotation(DisplayRotation::Rotate180)
        .connect_i2c(i2c)
        .into();
    display.init().unwrap();

    // Configure USB serial
    let usb_bus = UsbBusAllocator::new(UsbBus::new(
        pac.USBCTRL_REGS,
        pac.USBCTRL_DPRAM,
        clocks.usb_clock,
        true,
        &mut pac.RESETS,
    ));
    let mut serial = SerialPort::new(&usb_bus);
    let mut usb_dev = UsbDeviceBuilder::new(&usb_bus, UsbVidPid(0x16c0, 0x27dd))
        .product("Serial port")
        .device_class(USB_CLASS_CDC)
        .build();

    // enter loop
    let mut iter = [].iter();
    loop {
        // get next frame or restart iterator
        let Some(raw) = iter.next() else {
            iter = FRAMES.iter();
            continue;
        };

        // draw image
        let im = Image::new(raw, Point::new(0, 0));
        im.draw(&mut display).unwrap();
        display.flush().unwrap();

        // sleep for frame rate
        delay.start(1000.millis());
        let _ = nb::block!(delay.wait());

        // read and discard any serial data sent to us
        if usb_dev.poll(&mut [&mut serial]) {
            let mut buf = [0u8; 64];
            serial.read(&mut buf[..]).ok();
        }
    }
    */
}

enum Rotary {
    // 11 - default
    Rotary0,
    // 01 - starting clockwise
    Rotary1,
    // 00 - halfway
    Rotary2,
    // 10 - starting counter-clock
    Rotary3,
}

#[interrupt]
fn IO_IRQ_BANK0() {
    // The `#[interrupt]` attribute covertly converts this to `&'static mut Option<LedAndButton>`
    static mut LED_AND_BUTTON: Option<LedAndButton> = None;
    static mut ROTARY: Rotary = Rotary::Rotary0;
    static mut DIRECTION: Option<Direction> = None;

    // This is one-time lazy initialisation. We steal the variables given to us
    // via `GLOBAL_PINS`.
    if LED_AND_BUTTON.is_none() {
        critical_section::with(|cs| {
            *LED_AND_BUTTON = GLOBAL_PINS.borrow(cs).take();
        });
    }

    if let Some(gpios) = LED_AND_BUTTON {
        let (button1, button2, button3, button4) = gpios;

        button1.clear_interrupt(Interrupt::EdgeLow);
        button1.clear_interrupt(Interrupt::EdgeHigh);
        button2.clear_interrupt(Interrupt::EdgeLow);
        button2.clear_interrupt(Interrupt::EdgeHigh);

        critical_section::with(|cs| {
            let events = EVENTS.borrow(cs);

            events.replace_with(|events| {
                events[0] = if let Ok(true) = button1.is_high() {
                    0x31
                } else {
                    0x30
                };
                events[1] = if let Ok(true) = button2.is_high() {
                    0x31
                } else {
                    0x30
                };
                *events
            });

            if let (Ok(button1), Ok(button2)) = (button1.is_high(), button2.is_high()) {
                match (&ROTARY, &DIRECTION, button1, button2) {
                    // Rotate clockwise
                    (Rotary::Rotary0, _, false, true) => {
                        *DIRECTION = Some(Direction::Clockwise);
                        *ROTARY = Rotary::Rotary1;
                    }
                    (Rotary::Rotary3, Some(Direction::Clockwise), true, true) => {
                        ACTION
                            .borrow(cs)
                            .replace(Some(Action::Rotate(Direction::Clockwise)));
                        *ROTARY = Rotary::Rotary0;
                        *DIRECTION = None;
                    }

                    // Rotate counter-clock
                    (Rotary::Rotary0, _, true, false) => {
                        *DIRECTION = Some(Direction::CounterClock);
                        *ROTARY = Rotary::Rotary3;
                    }
                    (Rotary::Rotary1, Some(Direction::CounterClock), true, true) => {
                        ACTION
                            .borrow(cs)
                            .replace(Some(Action::Rotate(Direction::CounterClock)));
                        *ROTARY = Rotary::Rotary0;
                        *DIRECTION = None;
                    }

                    // misc
                    (_, _, true, true) => {
                        *ROTARY = Rotary::Rotary0;
                        *DIRECTION = None;
                    }
                    (_, _, false, true) => {
                        *ROTARY = Rotary::Rotary1;
                    }
                    (_, _, false, false) => {
                        *ROTARY = Rotary::Rotary2;
                    }
                    (_, _, true, false) => {
                        *ROTARY = Rotary::Rotary3;
                    }
                }
            }

            // reload
            if button3.interrupt_status(Interrupt::EdgeLow) {
                events.replace_with(|events| {
                    events[2] = 0x30;
                    *events
                });
                ACTION
                    .borrow(cs)
                    .replace(Some(Action::Press(Button::ReloadToggle)));
                button3.clear_interrupt(Interrupt::EdgeLow);
            }
            if button3.interrupt_status(Interrupt::EdgeHigh) {
                events.replace_with(|events| {
                    events[2] = 0x31;
                    *events
                });
                ACTION
                    .borrow(cs)
                    .replace(Some(Action::Release(Button::ReloadToggle)));
                button3.clear_interrupt(Interrupt::EdgeHigh);
            }

            // shoot
            if button4.interrupt_status(Interrupt::EdgeLow) {
                ACTION
                    .borrow(cs)
                    .replace(Some(Action::Press(Button::Shoot)));
                button4.clear_interrupt(Interrupt::EdgeLow);
            }
            if button4.interrupt_status(Interrupt::EdgeHigh) {
                ACTION
                    .borrow(cs)
                    .replace(Some(Action::Release(Button::Shoot)));
                button4.clear_interrupt(Interrupt::EdgeHigh);
            }
        });
    }
}
