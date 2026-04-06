use std::{
	io::{
		self,
		Write,
	},
	time::Duration,
};

use crossterm::{
	terminal::{
		is_raw_mode_enabled,
		Clear,
		ClearType,
	},
	ExecutableCommand,
};
use futures::{
	channel::mpsc::Receiver,
	executor::block_on,
	prelude::*,
};
use nodi::{
	midly::live::SystemRealtime,
	timers::Ticker,
	Connection,
	Event,
	Timer,
};

use crate::{
	track::Track,
	Command,
};

fn format_duration(t: Duration) -> String {
	let secs = t.as_secs();
	let mins = secs / 60;
	let secs = secs % 60;
	if mins > 0 {
		format!("{}m{}s", mins, secs)
	} else {
		format!("{}s", secs)
	}
}

enum Print {
	Append,
	ReplaceLast,
	ReplaceAll,
}

impl Print {
	fn print(self, s: &str) {
		fn inner(p: Print, s: &str) -> io::Result<()> {
			let mut stdout = io::stdout();
			match p {
				Print::ReplaceAll => {
					stdout.execute(Clear(ClearType::All))?;
				}
				Print::Append => writeln!(stdout)?,
				Print::ReplaceLast => {
					stdout.execute(Clear(ClearType::UntilNewLine))?;
				}
			}

			for (i, ln) in s.lines().enumerate() {
				if i > 0 {
					writeln!(stdout)?;
				}
				write!(stdout, "{ln}\r")?;
				stdout.flush()?;
			}
			Ok(())
		}

		if let Ok(true) = is_raw_mode_enabled() {
			let _ = inner(self, s);
		} else {
			println!("{s}");
		}
	}
}

fn gen_header(tracks: &[Track], speed: f32) -> String {
	let total_duration: Duration = tracks.iter().map(|t| t.duration).sum();
	let dur = Duration::from_micros((total_duration.as_micros() as f64 * speed as f64) as u64);
	format!(
		"Playing {n} track{s}
Total duration: {total}
Press the spacebar to play/pause, ctrl-left/right to play previous/next track
Press +/- to adjust the playback speed. Press the esc key or ctrl-c to exit",
		n = tracks.len(),
		s = if tracks.len() == 1 { "" } else { "s" },
		total = format_duration(dur)
	)
}

fn gen_status_line(track: &Track, speed: f32, bpm: Option<f64>) -> String {
	let dur = Duration::from_micros((track.duration.as_micros() as f64 * speed as f64) as u64);
	let bpm_str = match bpm {
		Some(b) => format!(" | {:.0} BPM", b),
		None => String::new(),
	};
	format!(
		"Duration = {} | Speed: {:.1}x{}",
		format_duration(dur),
		speed,
		bpm_str,
	)
}

fn make_display(
	header: &str,
	track: &Track,
	n_track: usize,
	total: usize,
	speed: f32,
	bpm: Option<f64>,
) -> String {
	format!(
		"{header}
Current: {name} [{n}/{total}]
{status}",
		name = track.name,
		n = n_track + 1,
		status = gen_status_line(track, speed, bpm),
	)
}

pub(crate) fn play<C: Connection>(
	mut con: C,
	tracks: &[Track],
	mut commands: Receiver<Command>,
	repeat: bool,
	mut speed: f32,
) {
	let mut n_track = 0;

	'outer: loop {
		// Reset the synth.
		con.send_sys_rt(SystemRealtime::Reset);
		// System synths seem to ignore the above so at least turn all notes off.
		con.all_notes_off();

		let mut counter = 0_u32;
		let track = &tracks[n_track];
		let mut timer = Ticker::new(track.tpb);
		timer.speed = speed;

		let mut current_bpm: Option<f64> = None;
		let header = gen_header(tracks, speed);
		Print::ReplaceAll.print(&make_display(&header, track, n_track, tracks.len(), speed, current_bpm));

		let mut paused = false;

		'track: for moment in track.sheet.iter() {
			match commands.try_next() {
				Err(_) => (),
				Ok(None) => break 'outer,
				Ok(Some(Command::Next)) => break 'track,
				Ok(Some(Command::Prev)) => {
					n_track = n_track.saturating_sub(1);
					continue 'outer;
				}
				Ok(Some(Command::SpeedUp)) => {
					speed = (speed + 0.1).min(10.0);
					timer.speed = speed;
					Print::ReplaceLast.print(&gen_status_line(track, speed, current_bpm));
				}
				Ok(Some(Command::SpeedDown)) => {
					speed = (speed - 0.1).max(0.1);
					timer.speed = speed;
					Print::ReplaceLast.print(&gen_status_line(track, speed, current_bpm));
				}
				Ok(Some(Command::Pause)) => {
					con.all_notes_off();
					if paused {
						Print::ReplaceLast.print("paused");
					} else {
						Print::Append.print("paused");
						paused = true;
					}
					// Wait for the next command, allowing speed changes while paused.
					loop {
						match block_on(commands.next()) {
							None => break 'outer,
							Some(Command::Pause) => break,
							Some(Command::Next) => break 'track,
							Some(Command::Prev) => {
								n_track = n_track.saturating_sub(1);
								continue 'outer;
							}
							Some(Command::SpeedUp) => {
								speed = (speed + 0.1).min(10.0);
								timer.speed = speed;
								Print::ReplaceLast
									.print(&format!("paused | Speed: {:.1}x", speed));
							}
							Some(Command::SpeedDown) => {
								speed = (speed - 0.1).max(0.1);
								timer.speed = speed;
								Print::ReplaceLast
									.print(&format!("paused | Speed: {:.1}x", speed));
							}
						}
					}

					// Redraw the full display after unpausing to show updated speed/BPM.
					let header = gen_header(tracks, speed);
					Print::ReplaceAll.print(&make_display(
						&header,
						track,
						n_track,
						tracks.len(),
						speed,
						current_bpm,
					));
				}
			};

			// Play the moment.
			if !moment.is_empty() {
				timer.sleep(counter);
				counter = 0;
			}
			for event in &moment.events {
				match event {
					Event::Tempo(val) => {
						timer.change_tempo(*val);
						let new_bpm = 60_000_000.0 / *val as f64;
						if current_bpm.map_or(true, |b| (b - new_bpm).abs() >= 0.5) {
							current_bpm = Some(new_bpm);
							Print::ReplaceLast
								.print(&gen_status_line(track, speed, current_bpm));
						}
					}
					Event::Midi(msg) => {
						con.play(*msg);
					}
					_ => (),
				};
			}

			counter += 1;
		}

		// Current track is over.
		n_track += 1;
		if n_track >= tracks.len() {
			if repeat {
				n_track = 0;
			} else {
				break;
			}
		}
	}

	con.send_sys_rt(SystemRealtime::Reset);
	con.all_notes_off();
}
