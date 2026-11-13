#![deny(unsafe_op_in_unsafe_fn)]
mod capture;
mod devices;
mod login_item;
mod notifications;
mod probe;
mod sharing_setup;
mod tray;
mod ui;
mod update_ui;
mod worker;

#[derive(Clone, Default)]
pub(crate) struct Runtime {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    update: std::sync::Arc<std::sync::Mutex<Option<mi_clip::updater::PendingUpdate>>>,
    install_update: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

use gpui_rhai::gpui;
use gpui_rhai::gpui::{App, KeyBinding, QuitMode, actions};
actions!(mi_clip, [ClosePanel, Quit]);

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--version"] {
        println!("Mi Clip {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.as_slice() == ["--check-update"] {
        println!(
            "Mi Clip {} · {}",
            env!("CARGO_PKG_VERSION"),
            mi_clip::updater::REPOSITORY
        );
        match mi_clip::updater::check()? {
            mi_clip::updater::Check::Current => println!("已是最新版本"),
            mi_clip::updater::Check::Unpublished => println!("更新尚未发布"),
            mi_clip::updater::Check::Ready(update) => println!(
                "发现并验证更新 {}；打开状态面板重新检查并安装。",
                update.version()
            ),
        }
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("--auth") {
        return mi_clip::auth_cli::run(&args[1..]);
    }
    if args.len() == 3 && args[0] == "--probe" {
        return probe::run(&args[1], args[2].parse()?);
    }
    let config = if args.len() == 2 && args[0] == "--config" {
        Some(std::path::PathBuf::from(&args[1]))
    } else {
        anyhow::ensure!(args.len() <= 1, "Use --help for usage");
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .map(|p| p.join("Library/Application Support/MiClip/config.json"))
            .filter(|p| p.is_file())
    };
    let argument = if args.len() == 2 {
        None
    } else {
        args.first().map(String::as_str)
    };
    if argument == Some("--check-ui") {
        ui::prepare()?;
        println!("UI scripts validated; no window created.");
        return Ok(());
    }
    if argument == Some("--help") {
        println!(
            "Mi Clip — standalone Xiaomi clipboard client\nNo main window. Click the menu-bar icon for status.\n--version: print the current version\n--check-update: check and verify an update without installing\n--config PATH: use a private account configuration\n--auth COMMAND INPUT OUTPUT: account setup and research CLI\n--check-ui: validate embedded Rhai scripts\n--smoke-test: exercise status button and popup lifecycle\n--probe HOST PORT: test device-info negotiation only\nSupports inline UTF-8 text up to 3 KiB, Mac file clipboard offers, and native phone file sharing to ~/Downloads. No vendor runtime required."
        );
        return Ok(());
    }
    let smoke = argument == Some("--smoke-test");
    let config = if smoke { None } else { config };
    if argument.is_some() && !smoke {
        anyhow::bail!("Unknown argument; use --help");
    }
    // An installed app must register its extension even when an older
    // standalone client already owns the clipboard lock.
    let share_setup = if smoke { None } else { sharing_setup::start() };
    // Hold an OS lock for this process's lifetime; no PID or clipboard data stored.
    let _instance = if !smoke {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let directory = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("Home directory unavailable"))?
            .join("Library/Application Support/MiClip");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join("instance.lock"))?;
        if file.try_lock().is_err() {
            if let Some(setup) = share_setup {
                let _ = setup.join();
            }
            return Ok(());
        }
        Some(file)
    } else {
        None
    };
    if !smoke {
        if let Some(home) = std::env::var_os("HOME") {
            mi_clip::analytics::start(
                std::path::PathBuf::from(home).join("Library/Application Support/MiClip"),
            );
        }
    }
    let passed = std::rc::Rc::new(std::cell::Cell::new(false));
    let signal = passed.clone();
    let runtime = Runtime::default();
    let background = runtime.clone();
    let update_args = args.clone();
    gpui_rhai::gpui_platform::application()
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            gpui_rhai::install(cx);
            if !smoke {
                capture::start();
            }
            let quitting = background.clone();
            let mut instance = _instance;
            // AppKit terminate exits the process; code after application.run
            // is not a shutdown hook on macOS. Finish replacement here, before
            // returning control to the native termination callback.
            cx.on_app_quit(move |_| {
                if !smoke {
                    capture::stop();
                }
                quitting
                    .stop
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                drop(instance.take());
                mi_clip::analytics::shutdown();
                if quitting
                    .install_update
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    if let Some(update) = quitting.update.lock().unwrap().take() {
                        if let Err(error) = mi_clip::updater::install(update, &update_args) {
                            eprintln!("Mi Clip update installation failed: {error:#}");
                        }
                    }
                }
                // Native termination does not run Rust destructors: remove a
                // downloaded update even when the user chooses plain Quit.
                drop(quitting.update.lock().unwrap().take());
                std::future::ready(())
            })
            .detach();
            cx.bind_keys([
                KeyBinding::new("escape", ClosePanel, None),
                KeyBinding::new("cmd-q", Quit, None),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            if let Err(error) = tray::install(
                cx,
                smoke.then_some(signal),
                if smoke { None } else { config },
                background,
            ) {
                eprintln!("Status item failed: {error}");
                cx.quit();
            } else if !smoke {
                login_item::ensure_enabled();
            }
        });
    anyhow::ensure!(!smoke || passed.get(), "Status panel smoke test failed");
    Ok(())
}
