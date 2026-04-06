use std::sync::{
	Arc,
	Mutex,
};

use eframe::egui;
use futures::channel::mpsc::Sender;

use crate::{
	playback::format_duration,
	Command,
	PlaybackState,
};

/// Run the egui/eframe GUI. Blocks until the window is closed.
pub fn run(
	state: Arc<Mutex<PlaybackState>>,
	cmd_tx: Sender<Command>,
) -> Result<(), Box<dyn std::error::Error>> {
	let options = eframe::NativeOptions {
		viewport: egui::ViewportBuilder::default()
			.with_inner_size([440.0, 320.0])
			.with_resizable(true)
			.with_title("plmidi"),
		..Default::default()
	};
	eframe::run_native(
		"plmidi",
		options,
		Box::new(move |_cc| Box::new(PlmidiApp::new(state, cmd_tx))),
	)
	.map_err(|e| format!("GUI error: {e}").into())
}

struct PlmidiApp {
	state: Arc<Mutex<PlaybackState>>,
	cmd_tx: Sender<Command>,
	/// Local speed value used by the slider; kept in sync with playback state.
	speed: f32,
}

impl PlmidiApp {
	fn new(state: Arc<Mutex<PlaybackState>>, cmd_tx: Sender<Command>) -> Self {
		let speed = state.lock().map(|g| g.speed).unwrap_or(1.0);
		Self {
			state,
			cmd_tx,
			speed,
		}
	}

	fn send(&mut self, cmd: Command) {
		let _ = self.cmd_tx.try_send(cmd);
	}
}

impl eframe::App for PlmidiApp {
	fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
		// Continuously repaint so the UI stays current.
		ctx.request_repaint();

		// Snapshot current playback state.
		let (playlist, track_idx, playback_speed, bpm, paused, done) = {
			let g = self.state.lock().unwrap();
			(
				g.playlist.clone(),
				g.track_index,
				g.speed,
				g.bpm,
				g.paused,
				g.done,
			)
		};

		// Keep local speed slider in sync when playback changes it (e.g. +/- keys in TUI).
		if (self.speed - playback_speed).abs() > 0.005 {
			self.speed = playback_speed;
		}

		egui::CentralPanel::default().show(ctx, |ui| {
			ui.heading("plmidi");
			ui.separator();

			// ── Track info ────────────────────────────────────────────────
			if done {
				ui.label("Playback finished.");
			} else if playlist.is_empty() {
				ui.label("Loading…");
			} else {
				let track_name = playlist
					.get(track_idx)
					.map(|(n, _)| n.as_str())
					.unwrap_or("—");
				ui.horizontal(|ui| {
					ui.strong("Now playing:");
					ui.label(format!(
						"{} [{}/{}]",
						track_name,
						track_idx + 1,
						playlist.len()
					));
				});

				if let Some(b) = bpm {
					ui.label(format!("BPM: {:.0}", b));
				}
			}

			ui.add_space(8.0);

			// ── Transport controls ────────────────────────────────────────
			ui.horizontal(|ui| {
				if ui.button("⏮  Prev").clicked() {
					self.send(Command::Prev);
				}

				let pause_label = if paused { "▶  Play" } else { "⏸  Pause" };
				if ui.button(pause_label).clicked() {
					self.send(Command::Pause);
				}

				if ui.button("⏭  Next").clicked() {
					self.send(Command::Next);
				}
			});

			ui.add_space(8.0);
			ui.separator();

			// ── Speed / Tempo control ─────────────────────────────────────
			ui.label("Playback Speed");
			ui.horizontal(|ui| {
				if ui
					.button("−")
					.on_hover_text("Decrease speed by 0.1×")
					.clicked()
				{
					self.send(Command::SpeedDown);
				}

				let slider = egui::Slider::new(&mut self.speed, 0.1_f32..=4.0_f32)
					.step_by(0.1)
					.suffix("×")
					.text("speed");
				if ui.add(slider).changed() {
					// Round to one decimal place to avoid float drift.
					let rounded = (self.speed * 10.0).round() / 10.0;
					self.speed = rounded;
					self.send(Command::SetSpeed(rounded));
				}

				if ui
					.button("+")
					.on_hover_text("Increase speed by 0.1×")
					.clicked()
				{
					self.send(Command::SpeedUp);
				}
			});

			// Reset speed to 1.0 button.
			if ui.small_button("Reset to 1.0×").clicked() {
				self.speed = 1.0;
				self.send(Command::SetSpeed(1.0));
			}

			// ── Playlist ──────────────────────────────────────────────────
			if !playlist.is_empty() {
				ui.add_space(8.0);
				ui.separator();
				ui.collapsing("Playlist", |ui| {
					egui::ScrollArea::vertical()
						.max_height(100.0)
						.show(ui, |ui| {
							for (i, (name, duration)) in playlist.iter().enumerate() {
								let label = if i == track_idx && !done {
									format!("▶ {} ({})", name, format_duration(*duration))
								} else {
									format!("   {} ({})", name, format_duration(*duration))
								};
								ui.label(label);
							}
						});
				});
			}
		});
	}
}
