//! A desktop window: play a cartridge with picture, sound, keyboard and game
//! controllers. megadrive-python's `frontend.py` (milestone 9), class `Frontend`;
//! [`crate::frontend`] holds its pure parts. Built with the `frontend` feature.
//!
//! The reference uses pygame-ce, which is SDL; this is SDL2 itself, through the `sdl2`
//! crate. The design is the reference's: a window that presents at the display's
//! refresh, nearest-neighbour scaling of one console frame, and timing driven by the
//! audio queue. Where it differs:
//! - **The sound queue is SDL's own** (`SDL_QueueAudio`), not a Python buffer that an
//!   audio callback drains. SDL pads with silence when it runs dry, as the reference's
//!   `SoundBuffer.fill` does, and `size()` is the reference's `len(self.sound)`.
//! - **A lock-up** (the board stopping on something it does not model) pauses the
//!   console and names it in the title bar, where the reference would raise.
//! - **Screenshots are stamped in UTC** ([`crate::frontend::stamp`]).
//!
//! Timing: SDL's audio thread drains a short queue, and the loop runs the console one
//! frame at a time only while that queue is short, so the console's own 59.92 Hz
//! (49.70 Hz PAL) sets the pace. Unlike the reference on PyPy, this board keeps up
//! with room to spare (README, "Speed").

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sdl2::audio::{AudioQueue, AudioSpecDesired};
use sdl2::controller::{Axis, Button, GameController};
use sdl2::event::Event;
use sdl2::keyboard::{Keycode, Scancode};
use sdl2::pixels::PixelFormatEnum;
use sdl2::render::{Canvas, Texture, TextureCreator};
use sdl2::video::{FullscreenType, Window, WindowContext};
use sdl2::{EventPump, GameControllerSubsystem, Sdl};

use crate::cartridge::{load_image, Cartridge};
use crate::frontend::{
    frame_bytes, layout, sound_samples, stamp, where_, BUFFER_FRAMES, KEYS, SAMPLE_RATE,
    VOLUME_RANGE_DB, VOLUME_STEP_DB,
};
use crate::io::{Device, DOWN, LEFT, RIGHT, UP};
use crate::machine::machine;
use crate::png::write_png;
use crate::System;

/// How the window is opened and the console built: the command line's options.
pub struct Options {
    pub rom: PathBuf,
    pub machine: String,
    pub scale: u32,
    pub audio: bool,
    pub fullscreen: bool,
    pub fm: bool,
    pub skip: u32,
    pub pads: [String; 2],
    /// The controller layout, by name ([`crate::frontend::LAYOUTS`]).
    pub layout: String,
    pub save: Option<PathBuf>,
    pub tmss_rom: Option<PathBuf>,
    /// The master volume, in dB over the board's mix ([`crate::frontend::VOLUME_DB`]).
    pub volume_db: f64,
}

/// One console in one window.
pub struct Frontend {
    options: Options,
    pub system: System,
    /// The last finished frame, rows of 0xRRGGBB.
    pub frame: Vec<Vec<u32>>,
    pub frame_size: (u32, u32),
    canvas: Canvas<Window>,
    textures: TextureCreator<WindowContext>,
    texture: Texture,
    events: EventPump,
    controller_system: GameControllerSubsystem,
    controllers: Vec<(u32, GameController)>,
    device: Option<AudioQueue<i16>>,
    keys: Vec<(u16, Vec<Scancode>)>,
    buttons: Vec<(u16, Button)>,
    pub paused: bool,
    pub fast: bool,
    locked_up: Option<String>,
    /// The master volume now, and until when the title bar shows it after a step.
    volume_db: f64,
    volume_shown_until: Option<Instant>,
    frames: u32,
    console_frames: u32,
    fps_since: Instant,
}

impl Frontend {
    pub fn new(sdl: &Sdl, options: Options) -> Result<Self, String> {
        let mut system = power_on(&options)?;
        // The renderer hands out finished frames only, and at power-on none has
        // finished; a black one stands in until a frame has.
        let mut frame = vec![vec![0u32; 320]; 224];
        for _ in 0..3 {
            if system.run_frame().is_err() {
                break;
            }
            let vdp = &system.board().vdp;
            let finished = vdp.renderer.frame_rgb(&vdp.s);
            if finished.first().is_some_and(|row| !row.is_empty()) {
                frame = finished;
                break;
            }
        }
        let frame_size = (frame[0].len() as u32, frame.len() as u32);
        let video = sdl.video()?;
        // Nearest-neighbour: a console pixel stays a sharp block at any scale.
        sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "0");
        let stem = stem(&options.rom);
        let mut window = video.window(&format!("{stem} — megadrive-rust"), 320, 224);
        window.position_centered();
        let window = window.build().map_err(|e| e.to_string())?;
        // Presenting waits for the display's refresh (vsync), so a frame is never shown
        // half-drawn.
        let canvas = window
            .into_canvas()
            .present_vsync()
            .build()
            .map_err(|e| e.to_string())?;
        let textures = canvas.texture_creator();
        let texture = textures
            .create_texture_streaming(PixelFormatEnum::ARGB8888, frame_size.0, frame_size.1)
            .map_err(|e| e.to_string())?;
        let keys = KEYS
            .iter()
            .map(|&(button, names)| {
                let codes = names
                    .iter()
                    .map(|name| {
                        Keycode::from_name(name)
                            .and_then(Scancode::from_keycode)
                            .ok_or(format!("no key {name:?}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((button, codes))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let buttons = layout(&options.layout)?
            .iter()
            .map(|&(button, name)| {
                Button::from_string(name)
                    .map(|b| (button, b))
                    .ok_or(format!("no controller button {name:?}"))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let device = if options.audio { open_audio(sdl) } else { None };
        let mut frontend = Frontend {
            system,
            frame,
            frame_size,
            canvas,
            textures,
            texture,
            events: sdl.event_pump()?,
            controller_system: {
                // SDL2 names a Nintendo pad's face buttons by their printed letters
                // unless told otherwise, swapping A with B and X with Y against the
                // positions the layouts name. Set before the subsystem starts.
                sdl2::hint::set("SDL_GAMECONTROLLER_USE_BUTTON_LABELS", "0");
                sdl.game_controller()?
            },
            controllers: Vec::new(),
            device,
            keys,
            buttons,
            paused: false,
            fast: false,
            locked_up: None,
            volume_db: options.volume_db,
            volume_shown_until: None,
            frames: 0,
            console_frames: 0,
            fps_since: Instant::now(),
            options,
        };
        frontend.open_window(frame_size)?;
        Ok(frontend)
    }

    /// Size the window to one frame at the scale: SDL scales the frame to the window on
    /// the GPU. Called again when the console changes mode.
    fn open_window(&mut self, size: (u32, u32)) -> Result<(), String> {
        self.texture = self
            .textures
            .create_texture_streaming(PixelFormatEnum::ARGB8888, size.0, size.1)
            .map_err(|e| e.to_string())?;
        self.canvas
            .set_logical_size(size.0, size.1)
            .map_err(|e| e.to_string())?;
        let window = self.canvas.window_mut();
        if self.options.fullscreen {
            window.set_fullscreen(FullscreenType::Desktop)?;
        } else if window.fullscreen_state() == FullscreenType::Off {
            let height = if size.1 < 400 { size.1 } else { size.1 / 2 }; // interlace mode 2
            let scale = self.options.scale;
            window
                .set_size(size.0 * scale, height * scale)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Save the battery RAM.
    pub fn close(&mut self) -> Result<(), String> {
        self.save_battery()
    }

    fn save_battery(&mut self) -> Result<(), String> {
        let cartridge = &self.system.board().dev.cartridge;
        if let (Some(path), false) = (&self.options.save, cartridge.battery().is_empty()) {
            std::fs::write(path, cartridge.save_bytes()).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// The console's own frame rate.
    pub fn rate(&self) -> f64 {
        if self.system.machine.pal {
            49.70
        } else {
            59.92
        }
    }

    /// Bytes of sound one frame makes: stereo 16-bit at the host rate.
    fn frame_bytes(&self) -> u32 {
        (f64::from(SAMPLE_RATE) / self.rate()) as u32 * 4
    }

    pub fn run_frame(&mut self, draw: bool) {
        self.apply_inputs();
        self.system.board().vdp.s.rendering = draw;
        if let Err(lockup) = self.system.run_frame() {
            eprintln!("locked up: {lockup}");
            self.locked_up = Some(lockup.to_string());
            self.paused = true;
            return;
        }
        let samples = self.system.take_audio();
        if let (Some(device), false) = (&self.device, self.fast) {
            // A device that refuses a queue is dropped silence, not a stopped console.
            let _ = device.queue_audio(&sound_samples(&samples, self.volume_db));
        }
        if draw {
            let vdp = &self.system.board().vdp;
            let finished = vdp.renderer.frame_rgb(&vdp.s);
            if finished.first().is_some_and(|row| !row.is_empty()) {
                self.frame = finished;
            }
        }
        self.console_frames += 1;
    }

    // -- input --------------------------------------------------------------------

    /// Pad buttons held on the keyboard, as the board's bit mask.
    fn keyboard(&self) -> u16 {
        let state = self.events.keyboard_state();
        self.keys
            .iter()
            .filter(|(_, codes)| codes.iter().any(|&c| state.is_scancode_pressed(c)))
            .fold(0, |mask, (button, _)| mask | button)
    }

    /// Pad buttons held on one game controller.
    fn controller(&self, controller: &GameController) -> u16 {
        let mut mask = self
            .buttons
            .iter()
            .filter(|(_, b)| controller.button(*b))
            .fold(0, |mask, (button, _)| mask | button);
        let stick_x = f64::from(controller.axis(Axis::LeftX)) / 32768.0;
        let stick_y = f64::from(controller.axis(Axis::LeftY)) / 32768.0;
        for (button, pad, stick) in [
            (UP, Button::DPadUp, stick_y < -0.5),
            (DOWN, Button::DPadDown, stick_y > 0.5),
            (LEFT, Button::DPadLeft, stick_x < -0.5),
            (RIGHT, Button::DPadRight, stick_x > 0.5),
        ] {
            if stick || controller.button(pad) {
                mask |= button;
            }
        }
        mask
    }

    pub fn apply_inputs(&mut self) {
        let first = self.keyboard()
            | self
                .controllers
                .first()
                .map_or(0, |(_, c)| self.controller(c));
        let second = self
            .controllers
            .get(1)
            .map_or(0, |(_, c)| self.controller(c));
        let ports = &mut self.system.board().dev.io.ports;
        for (port, mask) in ports.iter_mut().zip([first, second]) {
            if let Some(device) = &mut port.device {
                device.set_pressed(mask);
            }
        }
    }

    // -- events -------------------------------------------------------------------

    /// Handle one event; false to quit.
    fn handle(&mut self, event: Event) -> Result<bool, String> {
        match event {
            Event::Quit { .. } => return Ok(false),
            Event::ControllerDeviceAdded { which, .. } => {
                if let Ok(controller) = self.controller_system.open(which) {
                    self.controllers
                        .push((controller.instance_id(), controller));
                }
            }
            Event::ControllerDeviceRemoved { which, .. } => {
                self.controllers.retain(|(id, _)| *id != which);
            }
            Event::KeyDown {
                keycode: Some(key),
                repeat: false,
                ..
            } => match key {
                Keycode::Escape => return Ok(false),
                Keycode::F3 => {
                    self.paused = !self.paused;
                    self.clear_sound();
                }
                // The console's reset button: the 68000, the Z80 and the YM2612, not a
                // power cycle (System::press_reset). One press per key down.
                Keycode::Backspace => self.system.press_reset(None),
                Keycode::F5 => {
                    self.save_battery()?;
                    self.system = power_on(&self.options)?;
                    self.locked_up = None;
                    self.paused = false;
                    self.clear_sound();
                }
                Keycode::F11 => {
                    let window = self.canvas.window_mut();
                    let next = if window.fullscreen_state() == FullscreenType::Off {
                        FullscreenType::Desktop
                    } else {
                        FullscreenType::Off
                    };
                    window.set_fullscreen(next)?;
                }
                Keycode::F12 => self.screenshot()?,
                Keycode::Minus | Keycode::KpMinus => self.step_volume(-VOLUME_STEP_DB),
                Keycode::Equals | Keycode::Plus | Keycode::KpPlus => {
                    self.step_volume(VOLUME_STEP_DB);
                }
                Keycode::Tab => {
                    self.fast = true;
                    self.clear_sound();
                }
                _ => {}
            },
            Event::KeyUp {
                keycode: Some(Keycode::Tab),
                ..
            } => self.fast = false,
            _ => {}
        }
        Ok(true)
    }

    fn step_volume(&mut self, step: f64) {
        let (low, high) = VOLUME_RANGE_DB;
        self.volume_db = (self.volume_db + step).clamp(low, high);
        self.volume_shown_until = Some(Instant::now() + Duration::from_secs(2));
        println!("volume {:+.0} dB", self.volume_db);
    }

    fn clear_sound(&self) {
        if let Some(device) = &self.device {
            device.clear();
        }
    }

    fn sound_queued(&self) -> u32 {
        self.device.as_ref().map_or(0, |d| d.size())
    }

    fn screenshot(&self) -> Result<(), String> {
        let folder = Path::new("screenshots");
        std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = folder.join(format!("{}-{}.png", stem(&self.options.rom), stamp(now)));
        write_png(&path, &self.frame).map_err(|e| e.to_string())?;
        println!("saved {}", path.display());
        Ok(())
    }

    // -- drawing ------------------------------------------------------------------

    pub fn draw(&mut self) -> Result<(), String> {
        let size = (self.frame[0].len() as u32, self.frame.len() as u32);
        if size != self.frame_size {
            // H32/H40, V28/V30 or interlace changed.
            self.frame_size = size;
            self.open_window(size)?;
        }
        self.texture
            .update(None, &frame_bytes(&self.frame), size.0 as usize * 4)
            .map_err(|e| e.to_string())?;
        self.canvas.clear();
        self.canvas.copy(&self.texture, None, None)?;
        self.canvas.present(); // waits for the display's refresh
        self.frames += 1;
        let elapsed = self.fps_since.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            let speed = f64::from(self.console_frames) / elapsed / self.rate() * 100.0;
            let state = if let Some(lockup) = &self.locked_up {
                format!(" (locked up: {lockup})")
            } else if self.paused {
                String::from(" (paused)")
            } else if self.fast {
                String::from(" (fast)")
            } else {
                String::new()
            };
            let volume = match self.volume_shown_until {
                Some(until) if Instant::now() < until => {
                    format!(" — volume {:+.0} dB", self.volume_db)
                }
                _ => String::new(),
            };
            let title = format!(
                "{} — {} — {speed:.0}% speed, {:.0} fps{state}{volume} — megadrive-rust",
                stem(&self.options.rom),
                where_(self.system.board().vdp.s.frame, self.rate()),
                f64::from(self.frames) / elapsed,
            );
            self.canvas
                .window_mut()
                .set_title(&title)
                .map_err(|e| e.to_string())?;
            self.frames = 0;
            self.console_frames = 0;
            self.fps_since = Instant::now();
        }
        Ok(())
    }

    // -- the loop -----------------------------------------------------------------

    /// Run until the window closes, or until `max_frames` have been shown.
    pub fn run(&mut self, max_frames: Option<u64>) -> Result<(), String> {
        let target = BUFFER_FRAMES * self.frame_bytes();
        let mut shown = 0u64;
        let mut cycle = 0u64;
        let mut next_tick = Instant::now();
        loop {
            let events: Vec<Event> = self.events.poll_iter().collect();
            for event in events {
                if !self.handle(event)? {
                    return Ok(());
                }
            }
            let mut ran = false;
            if !self.paused {
                if self.fast {
                    for _ in 0..4 {
                        self.run_frame(false);
                    }
                    self.run_frame(true);
                    ran = true;
                } else if self.device.is_none() || self.sound_queued() < target {
                    // Audio sets the pace: a frame each time the queue is short of its
                    // target.
                    let draw = cycle % u64::from(self.options.skip + 1) == 0;
                    cycle += 1;
                    self.run_frame(draw);
                    ran = draw;
                }
            }
            // A paused console still redraws, so the title bar stays current.
            if ran || self.paused {
                self.draw()?;
            }
            if ran {
                shown += 1;
                if max_frames.is_some_and(|m| shown >= m) {
                    return Ok(());
                }
            }
            if (self.device.is_none() && !self.fast) || self.paused {
                // No sound to set the pace: the console's rate by the clock.
                next_tick +=
                    Duration::from_secs_f64(f64::from(self.options.skip + 1) / self.rate());
                let now = Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                } else {
                    next_tick = now;
                }
            } else if !ran {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
}

fn power_on(options: &Options) -> Result<System, String> {
    let machine =
        machine(&options.machine).ok_or(format!("unknown machine {}", options.machine))?;
    let mut cartridge = Cartridge::from_image(load_image(&options.rom)?);
    if let Some(path) = &options.save {
        if path.exists() && !cartridge.battery().is_empty() {
            cartridge.load_bytes(&std::fs::read(path).map_err(|e| e.to_string())?);
        }
    }
    let boot = match &options.tmss_rom {
        Some(path) => Some(load_image(path)?),
        None => None,
    };
    let pads = [
        Device::by_name(&options.pads[0])?,
        Device::by_name(&options.pads[1])?,
    ];
    let mut system = System::new(machine, Some(cartridge), Some(pads), boot);
    system.audio.sample_rate = SAMPLE_RATE;
    // With the generator off the FM's registers, timers and BUSY still run, so a game
    // behaves the same; only its sound is silent.
    system.board().dev.fm.enabled = options.fm;
    Ok(system)
}

fn open_audio(sdl: &Sdl) -> Option<AudioQueue<i16>> {
    let spec = AudioSpecDesired {
        freq: Some(SAMPLE_RATE as i32),
        channels: Some(2),
        samples: Some(512),
    };
    let queue = sdl
        .audio()
        .and_then(|audio| audio.open_queue::<i16, _>(None, &spec));
    match queue {
        Ok(queue) if queue.spec().freq == SAMPLE_RATE as i32 => {
            queue.resume();
            Some(queue)
        }
        Ok(queue) => {
            eprintln!("no sound: the device runs at {} Hz", queue.spec().freq);
            None
        }
        Err(error) => {
            eprintln!("no sound: {error}");
            None
        }
    }
}
