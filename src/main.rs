#[cfg(not(any(feature = "fluidlite", feature = "system")))]
compile_error!("you must enable at least one of fluid, fluid-bundled or system cargo features");

mod app;
#[cfg(feature = "fluidlite")]
mod fluid;
#[cfg(feature = "gui")]
mod gui;
mod playback;
mod track;

use std::{
	io::{
		self,
	},
	process,
	thread,
};

#[cfg(feature = "gui")]
use std::sync::{
	Arc,
	Mutex,
};

use cfg_if::cfg_if;
use crossterm::{
	event::{
		Event,
		EventStream,
		KeyCode,
		KeyEventKind,
		KeyModifiers,
		KeyboardEnhancementFlags,
		PopKeyboardEnhancementFlags,
		PushKeyboardEnhancementFlags,
	},
	execute,
	terminal::{
		disable_raw_mode,
		enable_raw_mode,
		EnterAlternateScreen,
		LeaveAlternateScreen,
	},
};
use futures::{
	channel::mpsc::{
		self,
		Receiver,
		Sender,
	},
	executor::block_on,
	prelude::*,
	select,
};
use log::{
	error,
	Level,
};
#[cfg(feature = "system")]
use nodi::midir::{
	MidiOutput,
	MidiOutputConnection,
};
use rand::prelude::SliceRandom;

use self::track::Track;

type Result<T, E = Box<dyn std::error::Error>> = ::std::result::Result<T, E>;

enum Command {
	Pause,
	Next,
	Prev,
	SpeedUp,
	SpeedDown,
	#[cfg_attr(not(feature = "gui"), allow(dead_code))]
	SetSpeed(f32),
	#[cfg(feature = "gui")]
	Load(Vec<Track>),
}

#[cfg(all(feature = "fluidlite", feature = "system"))]
enum Either<A, B> {
	Left(A),
	Right(B),
}

/// Shared playback state written by the playback thread and read by the GUI.
#[cfg(feature = "gui")]
pub struct PlaybackState {
	/// Playlist entries: (track name, duration). Set once before the playback thread starts.
	pub playlist: Vec<(String, std::time::Duration)>,
	pub track_index: usize,
	pub speed: f32,
	pub bpm: Option<f64>,
	pub paused: bool,
	pub done: bool,
}

fn init_logger(n: u64) -> Result<(), log::SetLoggerError> {
	let log = match n {
		0 => Level::Error,
		1 => Level::Warn,
		2 => Level::Info,
		_ => Level::Debug,
	};

	#[cfg(feature = "fluidlite")]
	{
		use fluidlite::LogLevel;
		struct L;
		impl fluidlite::Logger for L {
			fn log(&mut self, level: LogLevel, msg: &str) {
				match level {
					LogLevel::Error | LogLevel::Panic => log::error!(target: "fluidsynth", "{msg}"),
					LogLevel::Warning => log::warn!(target: "fluidsynth", "{msg}"),
					LogLevel::Info => log::info!(target: "fluidsynth", "{msg}"),
					_ => log::debug!(target: "fluidsynth", "{msg}"),
				}
			}
		}
		fluidlite::Log::set(LogLevel::DEBUG, L);
	}
	simple_logger::init_with_level(log)?;
	Ok(())
}

#[cfg(feature = "system")]
fn list_devices() -> Result<()> {
	let midi_out = MidiOutput::new("nodi")?;

	let out_ports = midi_out.ports();

	if out_ports.is_empty() {
		println!("No active MIDI output device detected.");
	} else {
		for (i, p) in out_ports.iter().enumerate() {
			println!(
				"#{}: {}",
				i,
				midi_out
					.port_name(p)
					.as_deref()
					.unwrap_or("<no device name>")
			);
		}
	}

	Ok(())
}

#[cfg(feature = "system")]
fn get_midi(n: usize) -> Result<MidiOutputConnection> {
	let midi_out = MidiOutput::new("nodi")?;

	let out_ports = midi_out.ports();
	if out_ports.is_empty() {
		return Err("no midi output device detected".into());
	}
	if n >= out_ports.len() {
		return Err(format!(
			"only {} devices detected; run with --list to see them",
			out_ports.len()
		)
		.into());
	}

	let out_port = &out_ports[n];
	let out = midi_out.connect(out_port, "plmidi")?;
	Ok(out)
}

fn run() -> Result<()> {
	#[cfg(feature = "gui")]
	let cmd = app::with_gui(app::new());
	#[cfg(not(feature = "gui"))]
	let cmd = app::new();

	let m = cmd.get_matches_from(wild::args());
	#[cfg(feature = "system")]
	if m.is_present("list") {
		return list_devices();
	}

	init_logger(m.occurrences_of("verbose"))?;

	let speed = m.value_of_t_or_exit::<f32>("speed");
	let repeat = m.is_present("repeat");
	let shuffle = m.is_present("shuffle");
	let transpose = m.value_of_t_or_exit::<i8>("transpose");

	// Extract connection-init params as owned, Send values so they can be moved
	// into a background thread if GUI mode is requested.
	#[cfg(feature = "fluidlite")]
	let soundfont = m.value_of("fluid").unwrap().to_string();
	#[cfg(feature = "system")]
	let device_arg = m.value_of_t::<usize>("device");

	let mut tracks = m
		.values_of("file")
		.into_iter()
		.flatten()
		.map(Track::new)
		.collect::<Result<Vec<_>, _>>()?;

	for t in &mut tracks {
		t.sheet.transpose(transpose, false);
	}
							Ok(n) => get_midi(n).map(Either::Right).map_err(|e| e.to_string()),
							Err(_) => {
								// No --device provided: try system MIDI device 1 first, then fall back to embedded fluid.
								match get_midi(1) {
									Ok(c) => Ok(Either::Right(c)),
									Err(_) => fluid::Fluid::new(&soundfont).map(Either::Left).map_err(|e| e.to_string()),
								}
							}
	if shuffle {
		tracks.shuffle(&mut rand::thread_rng());
	}

	// ── GUI mode ─────────────────────────────────────────────────────────────
	// When the `gui` feature is compiled in and the user passes `--gui`, spawn
	// playback on a background thread and run the egui window on the main thread.
	// The connection (fluid::Fluid / midir) is created INSIDE the background
	// thread because cpal::Stream is !Send on some platforms.
	#[cfg(feature = "gui")]
	if m.is_present("gui") {
		use std::sync::mpsc as sync_mpsc;

		let playlist = tracks
			.iter()
			.map(|t| (t.name.clone(), t.duration))
			.collect::<Vec<_>>();

		let state = Arc::new(Mutex::new(PlaybackState {
			playlist,
			speed,
			track_index: 0,
			bpm: None,
			paused: false,
			done: false,
		}));

		let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(4);
		let state_clone = Arc::clone(&state);
		let (init_tx, init_rx) = sync_mpsc::channel::<Result<(), String>>();

		thread::spawn(move || {
			cfg_if! {
				if #[cfg(all(feature = "fluidlite", feature = "system"))] {
					let con_res: Result<Either<fluid::Fluid, _>, String> = match device_arg {
						Err(_) => fluid::Fluid::new(&soundfont).map(Either::Left).map_err(|e| e.to_string()),
						Ok(n) => get_midi(n).map(Either::Right).map_err(|e| e.to_string()),
					};
					match con_res {
						// system-only: default to device 1 when not specified for GUI convenience.
						match get_midi(device_arg.unwrap_or(1)) {
							// Try to fall back to system MIDI if available.
							#[cfg(feature = "system")]
							match get_midi(device_arg.unwrap_or(0)) {
								Ok(con) => {
									let _ = init_tx.send(Ok(()));
									playback::play(con, tracks, cmd_rx, repeat, speed, Some(state_clone));
								}
								Err(e) => {
									log::warn!("failed to open MIDI device fallback: {}", e);
									let _ = init_tx.send(Ok(()));
								}
							}
							#[cfg(not(feature = "system"))]
							let _ = init_tx.send(Ok(()));
						}
						Ok(con) => {
							let _ = init_tx.send(Ok(()));
							match con {
								Either::Left(c) => playback::play(c, tracks, cmd_rx, repeat, speed, Some(state_clone)),
								Either::Right(c) => playback::play(c, tracks, cmd_rx, repeat, speed, Some(state_clone)),
							}
						}
					}
				} else if #[cfg(feature = "fluidlite")] {
					match fluid::Fluid::new(&soundfont) {
						Err(e) => {
							log::warn!("failed to load soundfont: {}", e.to_string());
							let _ = init_tx.send(Ok(()));
						}
						Ok(con) => {
							let _ = init_tx.send(Ok(()));
							playback::play(con, tracks, cmd_rx, repeat, speed, Some(state_clone));
						}
					}
				} else if #[cfg(feature = "system")] {
					// system-only: default_value("0") in app.rs means device_arg is always Ok.
					match get_midi(device_arg.unwrap_or(0)) {
						Err(e) => {
							log::warn!("failed to open MIDI device: {}", e.to_string());
							let _ = init_tx.send(Ok(()));
						}
						Ok(con) => {
							let _ = init_tx.send(Ok(()));
							playback::play(con, tracks, cmd_rx, repeat, speed, Some(state_clone));
						}
					}
				}
			}
		});

		// Wait for the playback thread to finish initialization before opening the GUI.
		match init_rx.recv() {
			Ok(Ok(())) => {}
			Ok(Err(e)) => return Err(e.into()),
			Err(_) => return Err("playback thread did not initialize".into()),
		}

		return gui::run(state, cmd_tx);
	}

	// ── TUI mode (default) ────────────────────────────────────────────────────
	// Create the connection on this thread (no Send requirement).
	cfg_if! {
		if #[cfg(all(feature = "fluidlite", feature = "system"))] {
			let con = match device_arg {
				Err(_) => Either::Left(fluid::Fluid::new(&soundfont)?),
				Ok(n) => Either::Right(get_midi(n)?),
			};
		} else if #[cfg(feature = "fluidlite")] {
			let con = fluid::Fluid::new(&soundfont)?;
		} else if #[cfg(feature = "system")] {
			// In the system-only build, app.rs sets .default_value("0") on --device,
			// so device_arg is always Ok. The unwrap_or(0) is a safety fallback only.
			let con = get_midi(device_arg.unwrap_or(0))?;
		} else {
			compile_error!("you must enable at least one of fluid, fluid-bundled or system cargo features");
		}
	}

	let (sender, receiver) = mpsc::channel(1);

	let (mut tx_done, rx_done) = mpsc::channel(0);
	let listen = thread::spawn(move || block_on(async move { listen_keys(sender, rx_done).await }));

	cfg_if! {
		if #[cfg(all(feature = "fluidlite", feature = "system"))] {
			match con {
				Either::Left(con) => playback::play(con, tracks, receiver, repeat, speed, None),
				Either::Right(con) => playback::play(con, tracks, receiver, repeat, speed, None),
			}
		} else {
			playback::play(con, tracks, receiver, repeat, speed, None);
		}
	}

	let _ = block_on(tx_done.send(()));
	let _ = listen.join();
	Ok(())
}

async fn listen_keys(mut sender: Sender<Command>, done: Receiver<()>) {
	let alt = execute!(io::stdout(), EnterAlternateScreen).is_ok();
	if let Err(e) = enable_raw_mode() {
		eprintln!("warning: failed to enable raw mode; hotkeys may not work properly: {e}");
	} else {
		let _ = execute!(
			io::stdout(),
			PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
		);
	}

	let mut events = EventStream::new()
		.take_while(|x| std::future::ready(x.is_ok()))
		.fuse();
	let mut done = done.fuse();

	let received_done = loop {
		let res = select! {
			_ = done.next() => break true,
			e = events.next() => e,
		};

		let should_break = match res {
			None => true,
			Some(Err(e)) => {
				error!("input error: {e}");
				true
			}
			Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => match k.code {
				KeyCode::Esc => true,
				KeyCode::Char('c' | 'd' | 'q') if k.modifiers == KeyModifiers::CONTROL => true,
				KeyCode::Left if k.modifiers == KeyModifiers::CONTROL => {
					sender.send(Command::Prev).await.is_err()
				}
				KeyCode::Right if k.modifiers == KeyModifiers::CONTROL => {
					sender.send(Command::Next).await.is_err()
				}
				KeyCode::Char(' ') => sender.send(Command::Pause).await.is_err(),
				KeyCode::Char('+' | '=') => sender.send(Command::SpeedUp).await.is_err(),
				KeyCode::Char('-') => sender.send(Command::SpeedDown).await.is_err(),
				_ => false,
			},
			_ => false,
		};
		if should_break {
			break false;
		}
	};

	if !received_done {
		drop(sender);
		let _ = done.next().await;
	}

	let _ = disable_raw_mode();
	if alt {
		let _ = execute!(io::stdout(), LeaveAlternateScreen);
		let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
	}
	process::exit(0);
}

fn main() {
	if let Err(e) = run() {
		eprintln!("error: {e}");
		process::exit(1);
	}
}
