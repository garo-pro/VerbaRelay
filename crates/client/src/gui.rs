//! Native setup and settings windows (wxWidgets via wxDragon).
//!
//! Both windows use standard controls only, each preceded by its text label, so
//! screen readers such as NVDA announce every field. Progress is shown in a
//! read-only text field that can be reviewed with the arrow keys, and errors
//! additionally open a message box so they are announced immediately.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use wxdragon::prelude::*;
use wxdragon::timer::Timer;

use crate::config::{ClientConfig, Paths};
use crate::detect::BackendOption;
use crate::setup::{self, SetupInput};

enum SetupEvent {
    Progress(String),
    Detected(Vec<BackendOption>, Option<String>),
    Finished(Result<ClientConfig, String>),
}

/// Whether a desktop session is available for windows.
pub fn available() -> bool {
    if cfg!(target_os = "linux") {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    } else {
        true
    }
}

fn label(parent: &Panel, sizer: &BoxSizer, text: &str) {
    let label = StaticText::builder(parent).with_label(text).build();
    sizer.add(&label, 0, SizerFlag::Left | SizerFlag::Right | SizerFlag::Top, 8);
}

fn add_field<W: WxWidget>(sizer: &BoxSizer, widget: &W) {
    sizer.add(
        widget,
        0,
        SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right | SizerFlag::Bottom,
        8,
    );
}

fn error_box(parent: &Frame, message: &str) {
    MessageDialog::builder(parent, message, "VerbaRelay")
        .with_style(MessageDialogStyle::OK | MessageDialogStyle::IconError)
        .build()
        .show_modal();
}

/// Makes Escape close the frame, the same as its close button.
fn close_on_escape(frame: Frame) {
    const WXK_ESCAPE: i32 = 27;
    frame.on_char_hook(move |event| {
        let key = match &event {
            WindowEventData::Keyboard(key) => key.get_key_code(),
            WindowEventData::General(general) => general.get_key_code(),
            _ => None,
        };
        if key == Some(WXK_ESCAPE) {
            frame.close(false);
        } else {
            event.skip(true);
        }
    });
}

/// Runs the setup window. Returns the saved configuration, or `None` if cancelled.
pub fn run_setup(paths: Paths) -> Option<ClientConfig> {
    let result: Arc<Mutex<Option<ClientConfig>>> = Arc::new(Mutex::new(None));
    let result_ui = result.clone();

    let _ = wxdragon::main(move |_| {
        let (tx, rx) = mpsc::channel::<SetupEvent>();
        let options: Rc<RefCell<Vec<BackendOption>>> = Rc::new(RefCell::new(Vec::new()));

        let frame = Frame::builder()
            .with_title("VerbaRelay setup")
            .with_size(Size::new(620, 560))
            .build();
        let panel = Panel::builder(&frame).build();
        let sizer = BoxSizer::builder(Orientation::Vertical).build();

        let intro = StaticText::builder(&panel)
            .with_label(
                "Connect this computer to a VerbaRelay server. Afterwards it transcribes audio in the background.",
            )
            .build();
        intro.wrap(580);
        sizer.add(&intro, 0, SizerFlag::All, 8);

        label(&panel, &sizer, "Server address:");
        let server = TextCtrl::builder(&panel).build();
        add_field(&sizer, &server);

        label(&panel, &sizer, "Master password:");
        let password = TextCtrl::builder(&panel).with_style(TextCtrlStyle::Password).build();
        add_field(&sizer, &password);

        label(&panel, &sizer, "Device name:");
        let (hostname, _, _, _) = crate::detect::host_info();
        let name = TextCtrl::builder(&panel).with_value(&hostname).build();
        add_field(&sizer, &name);

        label(&panel, &sizer, "Compute backend:");
        let backend = Choice::builder(&panel)
            .with_choices(vec!["Detecting hardware...".to_string()])
            .build();
        backend.set_selection(0);
        backend.enable(false);
        add_field(&sizer, &backend);

        label(&panel, &sizer, "Status:");
        let status = TextCtrl::builder(&panel)
            .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly)
            .with_size(Size::new(-1, 140))
            .build();
        sizer.add(
            &status,
            1,
            SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right | SizerFlag::Bottom,
            8,
        );

        let buttons = BoxSizer::builder(Orientation::Horizontal).build();
        let connect = Button::builder(&panel).with_label("Connect").build();
        let cancel = Button::builder(&panel).with_label("Cancel").build();
        connect.enable(false);
        buttons.add(&connect, 0, SizerFlag::All, 4);
        buttons.add(&cancel, 0, SizerFlag::All, 4);
        sizer.add_sizer(&buttons, 0, SizerFlag::AlignRight | SizerFlag::All, 4);

        panel.set_sizer(sizer, true);
        close_on_escape(frame);
        frame.show(true);
        frame.centre();
        server.set_focus();

        // Hardware detection with a short speed test runs right away.
        let detect_tx = tx.clone();
        let detect_paths = paths.clone();
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Runtime::new() else {
                return;
            };
            let progress_tx = detect_tx.clone();
            let progress = move |msg: String| {
                let _ = progress_tx.send(SetupEvent::Progress(msg));
            };
            let detection = runtime.block_on(setup::detect(&detect_paths, true, &progress));
            let _ = detect_tx.send(SetupEvent::Detected(detection.options, detection.note));
        });

        // Connect: enroll, install CUDA if chosen, save.
        let connect_tx = tx.clone();
        let connect_paths = paths.clone();
        let connect_options = options.clone();
        let frame_for_connect = frame;
        connect.on_click(move |_| {
            let server_url = server.get_value();
            let master_password = password.get_value();
            let device_name = name.get_value();
            if server_url.trim().is_empty() || master_password.is_empty() || device_name.trim().is_empty() {
                error_box(
                    &frame_for_connect,
                    "Please fill in the server address, the master password and the device name.",
                );
                return;
            }
            let Some(option) = backend
                .get_selection()
                .and_then(|i| connect_options.borrow().get(i as usize).cloned())
            else {
                error_box(&frame_for_connect, "Please choose a compute backend.");
                return;
            };
            connect.enable(false);
            let input = SetupInput {
                allow_insecure: server_url.trim().starts_with("http://"),
                server_url,
                master_password,
                device_name,
                backend: option.backend,
            };
            let tx = connect_tx.clone();
            let paths = connect_paths.clone();
            std::thread::spawn(move || {
                let outcome = match tokio::runtime::Runtime::new() {
                    Ok(runtime) => {
                        let progress_tx = tx.clone();
                        let progress = move |msg: String| {
                            let _ = progress_tx.send(SetupEvent::Progress(msg));
                        };
                        runtime
                            .block_on(setup::complete(&paths, input, &progress))
                            .map_err(|e| format!("{e:#}"))
                    }
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(SetupEvent::Finished(outcome));
            });
        });

        cancel.on_click(move |_| {
            frame.close(true);
        });

        // Events from the background threads are applied on the UI thread.
        let timer = Rc::new(Timer::new(&frame));
        let timer_keepalive = timer.clone();
        let result_ui = result_ui.clone();
        timer.on_tick(move |_| {
            let _keep = &timer_keepalive;
            while let Ok(event) = rx.try_recv() {
                match event {
                    SetupEvent::Progress(msg) => status.append_text(&format!("{msg}\n")),
                    SetupEvent::Detected(found, note) => {
                        if let Some(note) = note {
                            status.append_text(&format!("{note}\n"));
                        }
                        backend.clear();
                        for option in &found {
                            backend.append(&option.label());
                        }
                        let recommended = found.iter().position(|o| o.recommended).unwrap_or(0);
                        backend.set_selection(recommended as u32);
                        backend.enable(true);
                        connect.enable(true);
                        *options.borrow_mut() = found;
                    }
                    SetupEvent::Finished(Ok(config)) => {
                        *result_ui.lock().unwrap() = Some(config);
                        frame.close(true);
                    }
                    SetupEvent::Finished(Err(message)) => {
                        status.append_text(&format!("Error: {message}\n"));
                        connect.enable(true);
                        error_box(&frame, &message);
                    }
                }
            }
        });
        timer.start(100, false);
    });

    result.lock().unwrap().take()
}

/// Runs the settings window: update interval and development builds.
pub fn run_settings(paths: Paths) {
    let _ = wxdragon::main(move |_| {
        let config = match ClientConfig::load(&paths.config_file) {
            Ok(c) => c,
            Err(e) => {
                let frame = Frame::builder().with_title("VerbaRelay").build();
                error_box(&frame, &format!("The configuration could not be read: {e:#}"));
                frame.close(true);
                return;
            }
        };

        let frame = Frame::builder()
            .with_title("VerbaRelay settings")
            .with_size(Size::new(560, 460))
            .build();
        let panel = Panel::builder(&frame).build();
        let sizer = BoxSizer::builder(Orientation::Vertical).build();

        label(&panel, &sizer, "Status:");
        let status = TextCtrl::builder(&panel)
            .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly)
            .with_value(&status_text(&paths, &config))
            .with_size(Size::new(-1, 170))
            .build();
        sizer.add(
            &status,
            1,
            SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right | SizerFlag::Bottom,
            8,
        );

        label(&panel, &sizer, "Check for updates every (hours):");
        let interval = SpinCtrl::builder(&panel).with_range(1, 720).build();
        interval.set_value(config.updates.check_interval_hours as i32);
        add_field(&sizer, &interval);

        let dev = CheckBox::builder(&panel)
            .with_label("Also install development builds")
            .build();
        dev.set_value(config.updates.include_dev_builds);
        add_field(&sizer, &dev);

        let buttons = BoxSizer::builder(Orientation::Horizontal).build();
        let save = Button::builder(&panel).with_label("Save").build();
        let close = Button::builder(&panel).with_label("Close").build();
        buttons.add(&save, 0, SizerFlag::All, 4);
        buttons.add(&close, 0, SizerFlag::All, 4);
        sizer.add_sizer(&buttons, 0, SizerFlag::AlignRight | SizerFlag::All, 4);

        panel.set_sizer(sizer, true);
        close_on_escape(frame);
        frame.show(true);
        frame.centre();
        interval.set_focus();

        let save_paths = paths.clone();
        save.on_click(move |_| {
            let mut config = match ClientConfig::load(&save_paths.config_file) {
                Ok(c) => c,
                Err(e) => {
                    error_box(&frame, &format!("The configuration could not be read: {e:#}"));
                    return;
                }
            };
            config.updates.check_interval_hours = interval.value().clamp(1, 720) as u32;
            config.updates.include_dev_builds = dev.get_value();
            if let Err(e) = config.save(&save_paths.config_file) {
                error_box(&frame, &format!("Saving failed: {e:#}"));
                return;
            }
            // Tell the running client; if none runs, the next start picks it up.
            let _ = crate::ipc::send_blocking(&save_paths, "reload");
            frame.close(true);
        });
        close.on_click(move |_| {
            frame.close(true);
        });
    });
}

/// Human readable status of the running client.
pub fn status_text(paths: &Paths, config: &ClientConfig) -> String {
    let info = crate::release::BuildInfo::current();
    let mut lines = vec![format!("Version: {}", info.describe())];
    match crate::ipc::agent_status(paths) {
        Ok(s) => {
            lines.push(format!(
                "Server: {} ({})",
                s.server,
                if s.connected { "connected" } else { "not connected" }
            ));
            lines.push(format!("Device: {} via {}", s.accelerator, s.backend));
            lines.push(format!(
                "Transcription score: {}",
                s.score
                    .map_or_else(|| "not measured in this session".into(), |v| format!("{v:.1}"))
            ));
            lines.push(format!("Currently working: {}", if s.working { "yes" } else { "no" }));
            lines.push(format!("Jobs finished since start: {}", s.jobs_done));
            if let Some(update) = s.pending_update {
                lines.push(format!("Update waiting to be installed: {update}"));
            }
            if let Some(error) = s.last_error {
                lines.push(format!("Last problem: {error}"));
            }
        }
        Err(_) => {
            lines.push(format!("Server: {}", config.server_url));
            lines.push("The background client is not running.".into());
        }
    }
    if !info.updates_configured() {
        lines.push("Automatic updates are not available in this build.".into());
    }
    lines.join("\n")
}
