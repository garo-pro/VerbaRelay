//! Tray icon of the running client.
//!
//! The agent starts it as a separate process (`verbarelay-client tray`), so the
//! agent itself never runs a GUI loop. The icon talks to the agent over the
//! local control channel and shows its state in the tooltip. Started by the
//! agent it exits when its stdin closes, which happens however the agent ends;
//! started by hand it exits when the agent stops answering. Its menu opens the
//! settings, checks for updates now and stops the client.

use std::cell::Cell;
use std::io::Read;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use wxdragon::prelude::*;
use wxdragon::timer::Timer;

use crate::config::Paths;
use crate::ipc::{self, AgentStatus};

const ID_SETTINGS: Id = ID_HIGHEST + 1;
const ID_CHECK_UPDATES: Id = ID_HIGHEST + 2;
const ID_EXIT: Id = ID_HIGHEST + 3;

/// How often the agent's status is polled.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Failed polls in a row after which the agent counts as gone.
const MAX_MISSED_POLLS: u32 = 2;

enum TrayEvent {
    Status(AgentStatus),
    AgentGone,
    UpdateAnswer(String),
}

/// Runs the tray icon until the agent stops or the user exits.
pub fn run(paths: Paths, attached: bool) {
    let _ = wxdragon::main(move |app| {
        // Closing the settings window must not end the tray.
        app.set_exit_on_frame_delete(false);
        let (tx, rx) = mpsc::channel::<TrayEvent>();

        // Never shown: the parent of message boxes and the owner of the timer.
        let host = Frame::builder().with_title("VerbaRelay").build();
        let icon = Rc::new(TaskBarIcon::builder().build());
        let bitmap = Rc::new(Bitmap::from_rgba(&icon_rgba(), ICON_SIZE, ICON_SIZE));
        let set_tooltip = {
            let icon = icon.clone();
            let bitmap = bitmap.clone();
            move |text: &str| {
                if let Some(bitmap) = bitmap.as_ref() {
                    icon.set_icon(bitmap, text);
                }
            }
        };
        set_tooltip("VerbaRelay");

        let mut menu = Menu::builder()
            .append_item(ID_SETTINGS, "Settings...", "")
            .append_item(ID_CHECK_UPDATES, "Check for updates now", "")
            .append_separator()
            .append_item(ID_EXIT, "Exit", "")
            .build();
        icon.set_popup_menu(&mut menu);

        let settings: Rc<Cell<Option<Frame>>> = Rc::new(Cell::new(None));
        let open_settings = {
            let settings = settings.clone();
            let paths = paths.clone();
            move || match settings.get().filter(|f| f.is_valid()) {
                Some(frame) => {
                    frame.show(true);
                    frame.raise();
                }
                None => settings.set(crate::gui::settings_window(&paths)),
            }
        };
        let quit = {
            let icon = icon.clone();
            let done = Rc::new(Cell::new(false));
            move || {
                if done.replace(true) {
                    return;
                }
                icon.remove_icon();
                icon.destroy();
                host.destroy();
                app.exit_main_loop();
            }
        };

        let checking = Rc::new(Cell::new(false));
        icon.on_menu({
            let open_settings = open_settings.clone();
            let quit = quit.clone();
            let checking = checking.clone();
            let paths = paths.clone();
            let tx = tx.clone();
            move |event| match event.get_id() {
                ID_SETTINGS => open_settings(),
                ID_CHECK_UPDATES => {
                    if checking.replace(true) {
                        return;
                    }
                    let paths = paths.clone();
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let answer = ipc::send_blocking(&paths, "check-updates")
                            .unwrap_or_else(|e| format!("The update check failed: {e:#}"));
                        let _ = tx.send(TrayEvent::UpdateAnswer(answer));
                    });
                }
                ID_EXIT => {
                    // Remove the icon first: the agent ending closes our stdin.
                    quit();
                    let _ = ipc::send_blocking(&paths, "quit");
                }
                _ => {}
            }
        });
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        icon.on_left_double_click(move |_| open_settings());

        if attached {
            let gone_tx = tx.clone();
            std::thread::spawn(move || {
                let _ = std::io::stdin().read_to_end(&mut Vec::new());
                let _ = gone_tx.send(TrayEvent::AgentGone);
            });
        }

        // Watch the agent from a background thread.
        let poll_paths = paths.clone();
        std::thread::spawn(move || {
            let mut missed = 0;
            loop {
                let event = match ipc::agent_status(&poll_paths) {
                    Ok(status) => {
                        missed = 0;
                        TrayEvent::Status(status)
                    }
                    Err(_) => {
                        missed += 1;
                        if missed < MAX_MISSED_POLLS {
                            std::thread::sleep(POLL_INTERVAL);
                            continue;
                        }
                        TrayEvent::AgentGone
                    }
                };
                let gone = matches!(event, TrayEvent::AgentGone);
                if tx.send(event).is_err() || gone {
                    return;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        });

        let timer = Rc::new(Timer::new(&host));
        let timer_keepalive = timer.clone();
        let mut tooltip = String::new();
        timer.on_tick(move |_| {
            while let Ok(event) = rx.try_recv() {
                match event {
                    TrayEvent::Status(status) => {
                        let text = tooltip_text(&status);
                        if text != tooltip {
                            set_tooltip(&text);
                            tooltip = text;
                        }
                    }
                    TrayEvent::AgentGone => quit(),
                    TrayEvent::UpdateAnswer(answer) => {
                        checking.set(false);
                        // No ticks inside the modal loop: quitting would destroy the box's parent.
                        timer_keepalive.stop();
                        MessageDialog::builder(&host, &answer, "VerbaRelay updates")
                            .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconInformation)
                            .build()
                            .show_modal();
                        timer_keepalive.start(200, false);
                    }
                }
            }
        });
        timer.start(200, false);
    });
}

fn tooltip_text(status: &AgentStatus) -> String {
    let state = if !status.connected {
        "not connected"
    } else if status.working {
        "transcribing"
    } else {
        "ready"
    };
    let mut text = format!("VerbaRelay: {state}");
    if status.pending_update.is_some() {
        text.push_str(", update waiting");
    }
    text
}

const ICON_SIZE: u32 = 32;

/// Draws the tray icon: a blue disc with white level bars, anti-aliased by
/// 4x4 supersampling.
fn icon_rgba() -> Vec<u8> {
    const SAMPLES: u32 = 4;
    const BARS: [f32; 5] = [7.0, 13.0, 19.0, 13.0, 7.0];
    let size = ICON_SIZE as f32;
    let center = size / 2.0;
    let bar_width = 2.6;
    let bar_step = 4.6;
    let inside_bar = |x: f32, y: f32| {
        BARS.iter().enumerate().any(|(i, &height)| {
            let bar_x = center + (i as f32 - 2.0) * bar_step;
            (x - bar_x).abs() <= bar_width / 2.0 && (y - center).abs() <= height / 2.0
        })
    };
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for py in 0..ICON_SIZE {
        for px in 0..ICON_SIZE {
            let (mut disc, mut bar) = (0u32, 0u32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = px as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                    if (x - center).hypot(y - center) <= center - 0.5 {
                        disc += 1;
                        if inside_bar(x, y) {
                            bar += 1;
                        }
                    }
                }
            }
            let total = (SAMPLES * SAMPLES) as f32;
            let white = bar as f32 / disc.max(1) as f32;
            let mix = |blue: f32| (blue + (255.0 - blue) * white).round() as u8;
            rgba.extend_from_slice(&[mix(0x2F as f32), mix(0x6F as f32), mix(0xDE as f32)]);
            rgba.push((disc as f32 / total * 255.0).round() as u8);
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_transparent_corners_and_an_opaque_middle() {
        let rgba = icon_rgba();
        assert_eq!(rgba.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        assert_eq!(rgba[3], 0);
        let middle = ((ICON_SIZE / 2 * ICON_SIZE + ICON_SIZE / 2) * 4) as usize;
        assert_eq!(rgba[middle + 3], 255);
    }
}
