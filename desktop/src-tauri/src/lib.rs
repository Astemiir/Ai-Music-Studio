//! Desktop shell for Ai Music Studio.
//!
//! One window, and one of two studio services running as a process of its own:
//! the YuE2 service (`yue2-service.exe`, this crate) or the Music 3 service
//! (`mm3-service.exe`, built from the MiniMax studio). Each service supervises
//! its own engine - `yue2.cpp` or `minimaxmusic.cpp` - and listens on the port
//! the window talks to, so the interface does not care which engine answers.
//!
//! They are separate processes rather than one service hosted inside the shell
//! because the user may switch engines: the service holding the port has to be
//! stopped and the other started in its place, and a service living in this
//! process could not be stopped without closing the window. A compatible
//! service already listening on loopback - a developer running it separately -
//! is still used instead of starting a second one.

use std::{
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const SERVER_PORT: u16 = 8791;
/// The engine this studio is built around, and the one it runs until the user
/// picks another. `yue2` is this crate's own service; `mm3` is the second
/// service started in its place (see `chosen_engine`).
const YUE2_ENGINE: &str = "yue2";
const MM3_ENGINE: &str = "mm3";
/// The Music 3 service, built from the MiniMax studio and shipped beside the
/// executable.
const MUSIC3_SERVICE: &str = "mm3-service.exe";
/// The YuE2 service, built from this crate and shipped beside the executable.
/// It runs as a process of its own so the other engine can take the port
/// without the window being closed.
const YUE2_SERVICE: &str = "yue2-service.exe";

/// The service process the window started, whichever engine it belongs to.
///
/// Held so the engine switch can stop it: the replacement has to be the
/// process holding the port, and nothing else can stop a service but its
/// owner. The service takes its engine down with it - the engine lives in a
/// kill-on-close job object of the service's own - so killing one process is
/// the whole of stopping an engine.
static ENGINE_SERVICE: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);

const RELEASES_URL: &str = "https://github.com/timoncool/YuE2-Studio/releases/latest";
const STUDIO_DATA_DIRECTORY: &str = "Ai Music Studio";

/// Mutable studio data must not be derived from the process working directory
/// or from the installed executable directory: a Start Menu shortcut is free
/// to choose either of those. Environment overrides remain authoritative for
/// portable/development installations.
fn studio_data_directory() -> PathBuf {
    // A portable copy keeps everything it owns beside itself: models, the
    // library, media, logs and settings. Nothing is written into AppData, so
    // deleting the folder deletes the studio, and carrying the folder to
    // another machine carries the whole studio with it.
    if is_portable() {
        return executable_directory().join("data");
    }

    // An installation is treated the same way whenever it can be: someone who
    // installs into F:\AI expects the twenty-five gigabytes of weights to land
    // in F:\AI, not in their profile on C:. Only when the install directory
    // cannot be written to - Program Files, a read-only share - does the studio
    // fall back to AppData, because then it has nowhere else to go.
    let beside_the_executable = executable_directory().join("data");
    if directory_is_writable(&beside_the_executable) {
        return beside_the_executable;
    }

    #[cfg(windows)]
    {
        if let Some(root) = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA")) {
            return PathBuf::from(root).join(STUDIO_DATA_DIRECTORY);
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(root) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(root).join("ai-music-studio");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("ai-music-studio");
        }
    }

    std::env::temp_dir().join("ai-music-studio")
}

/// A portable copy, or an installation into a folder it can write to: then the
/// data, the temporary files and the WebView2 profile all live beside the
/// executable, and deleting the folder deletes the studio.
fn keeps_everything_beside_itself() -> bool {
    studio_data_directory().starts_with(executable_directory())
}

/// Sets the same deterministic roots before the Axum bridge is spawned. Its
/// environment is inherited by the bridge and the subsequently started native
/// engine, so both always refer to one model library.
fn configure_studio_runtime_paths() {
    let data_root = studio_data_directory();

    // Temporary files count as leaving traces too: the engine, the downloader
    // and ffmpeg all write through the system temporary directory, and a
    // studio that keeps its data beside itself has no business filling the
    // system drive with them.
    if keeps_everything_beside_itself() {
        let temporary = executable_directory().join("temp");
        let _ = std::fs::create_dir_all(&temporary);
        for variable in ["TEMP", "TMP"] {
            unsafe {
                std::env::set_var(variable, &temporary);
            }
        }
    }
    if std::env::var_os("YUE_MODELS_ROOT").is_none() {
        unsafe {
            std::env::set_var(
                "YUE_MODELS_ROOT",
                data_root.join("models").join("yue2-cpp"),
            );
        }
    }
    if std::env::var_os("YUE_STUDIO_SETTINGS_PATH").is_none() {
        unsafe {
            std::env::set_var("YUE_STUDIO_SETTINGS_PATH", data_root.join("studio-settings.json"));
        }
    }
    // Without this the service falls back to `<working directory>/data` for the
    // library and media files. A Start Menu shortcut does not control the
    // working directory, so an installed build would scatter or lose the user's
    // library depending on how it was launched.
    if std::env::var_os("YUE_STUDIO_DATA_ROOT").is_none() {
        unsafe {
            std::env::set_var("YUE_STUDIO_DATA_ROOT", &data_root);
        }
    }

    // The service resolves and supervises the engine; the shell only needs the
    // loopback address they agree on.
    let host = std::env::var("YUE_ENGINE_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("YUE_ENGINE_PORT").ok().and_then(|value| value.parse::<u16>().ok()).unwrap_or(music_engine::yue_server::DEFAULT_PORT);
    if std::env::var_os("YUE_ENGINE_BASE_URL").is_none() {
        unsafe {
            std::env::set_var("YUE_ENGINE_BASE_URL", format!("http://{host}:{port}"));
        }
    }
}

fn service_is_ready() -> bool {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, SERVER_PORT);
    TcpStream::connect_timeout(&address.into(), Duration::from_millis(150)).is_ok()
}

fn wait_until_ready(timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if service_is_ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    false
}

/// Waits for the studio port to be free, which is how a stopped service
/// announces that its replacement may take it.
fn wait_until_port_free(timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !service_is_ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    false
}

fn executable_directory() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn is_portable() -> bool {
    executable_directory().join("portable.flag").is_file()
}

/// Whether the studio may keep its data here.
///
/// Asked by creating the directory and writing to it, because that is the only
/// answer that matters: permissions on Windows are not something to reason
/// about from a path.
fn directory_is_writable(path: &std::path::Path) -> bool {
    if std::fs::create_dir_all(path).is_err() {
        return false;
    }
    let probe = path.join(".write-probe");
    let written = std::fs::write(&probe, b"1").is_ok();
    let _ = std::fs::remove_file(&probe);
    written
}

/// Hosts the studio service inside this process.
///
/// This is the fallback for a build shipped without `yue2-service.exe`: the
/// studio then runs its own service as before, and only the engine switch is
/// the poorer for it (a service that lives in the window cannot be stopped
/// without closing it, so switching restarts the studio). The service is
/// started on its own runtime thread and the window only opens once it answers
/// on loopback.
fn start_service() -> Result<(), String> {
    if service_is_ready() {
        return Ok(());
    }

    std::thread::Builder::new()
        .name("music-server".into())
        .spawn(|| {
            let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("could not create the studio service runtime: {error}");
                    return;
                }
            };
            // Closing the studio and opening it again leaves the previous
            // process holding the port for a moment. Giving up on the first
            // refusal left the window on a browser error page with no way back
            // except restarting the application.
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                match runtime.block_on(music_server::serve()) {
                    Ok(()) => return,
                    Err(error) if Instant::now() < deadline => {
                        eprintln!("studio service could not start yet: {error}");
                        std::thread::sleep(Duration::from_millis(400));
                    }
                    Err(error) => {
                        eprintln!("studio service stopped: {error}");
                        return;
                    }
                }
            }
        })
        .map_err(|error| format!("could not start the studio service thread: {error}"))?;

    if wait_until_ready(Duration::from_secs(30)) {
        Ok(())
    } else {
        Err(format!("the studio service did not become ready on 127.0.0.1:{SERVER_PORT}"))
    }
}

/// The engine this studio runs.
///
/// The studio is YuE2 unless the user chose otherwise: its own service is the
/// one compiled into this executable, and it supervises `yue-server`. Choosing
/// Music 3 starts the second service instead, which brings `mm-server` and the
/// Music 3 model set with it.
fn chosen_engine() -> String {
    std::fs::read_to_string(engine_choice_path())
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("engine").and_then(|engine| engine.as_str()).map(str::to_owned))
        .filter(|engine| !engine.is_empty())
        .unwrap_or_else(|| YUE2_ENGINE.to_owned())
}

/// Where the engine this studio runs is remembered between starts.
fn engine_choice_path() -> PathBuf {
    studio_data_directory().join("studio-engine.json")
}

/// The engine this window is running, so closing it can write the choice down.
static CURRENT_ENGINE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn remember_engine(engine: &str) {
    if let Ok(mut current) = CURRENT_ENGINE.lock() {
        *current = Some(engine.to_owned());
    }
}

fn current_engine() -> Option<String> {
    CURRENT_ENGINE.lock().ok().and_then(|current| current.clone())
}

/// The command that starts the service of `engine`.
///
/// Each service is told where everything it owns lives: the data folder the
/// studio uses, the engine bundle and model set beside it, and a settings file
/// of its own - the two engines keep different engine options (YuE2 has the VAE
/// tiling knobs, Music 3 has split CFG forwards), so neither may read the
/// other's. Its output goes to a log of its own in `data`.
fn engine_service_command(engine: &str) -> Result<std::process::Command, String> {
    let root = executable_directory();
    let data = studio_data_directory();
    let (service, log_name) = if engine == MM3_ENGINE {
        (MUSIC3_SERVICE, "mm3-service.log")
    } else {
        (YUE2_SERVICE, "yue2-service.log")
    };
    let program = root.join(service);
    if !program.is_file() {
        return Err(format!("the {engine} service is missing: {}", program.display()));
    }

    let mut command = std::process::Command::new(&program);
    command.current_dir(&root).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Started from a windowed process, a console program opens a console
        // window of its own: it would sit beside the studio and flash on every
        // engine switch. Its output goes to the log file either way.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    if engine == MM3_ENGINE {
        command
            .env("MINIMAX_STUDIO_PORT", SERVER_PORT.to_string())
            .env("MINIMAX_STUDIO_DATA_ROOT", &data)
            .env("MINIMAX_MM_SERVER_ROOT", root.join("resources").join("minimaxmusic-cpp"))
            .env("MINIMAX_MUSIC_MODELS_ROOT", data.join("models").join("minimaxmusic-cpp"))
            .env("MINIMAX_STUDIO_SETTINGS_PATH", data.join("studio-settings-mm3.json"));
    } else {
        let models = std::env::var_os("YUE_MODELS_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| data.join("models").join("yue2-cpp"));
        command
            .env("YUE_STUDIO_PORT", SERVER_PORT.to_string())
            .env("YUE_STUDIO_DATA_ROOT", &data)
            .env("YUE_MODELS_ROOT", models)
            .env("YUE_STUDIO_SETTINGS_PATH", data.join("studio-settings.json"));
    }

    // Without a log file the service's output would go nowhere at all.
    match std::fs::File::create(data.join(log_name)) {
        Ok(file) => match file.try_clone() {
            Ok(copy) => {
                command.stdout(file).stderr(copy);
            }
            Err(_) => {
                command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            }
        },
        Err(_) => {
            command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        }
    }
    Ok(command)
}

/// Empties the engine logs of this data folder.
///
/// An engine appends to its own log for as long as it runs and only trims it
/// once it is large, so without this a log would outlive session after session
/// and stop being something to read. One window is one session: the files are
/// emptied here, when the studio starts, and not when the engine is switched -
/// the log of the engine that ran a minute ago is still wanted then.
fn clear_engine_logs(data: &Path) {
    for name in ["engine.log", "engine-startup.log"] {
        let path = data.join(name);
        if path.is_file() && std::fs::File::create(&path).is_err() {
            eprintln!("could not empty {}", path.display());
        }
    }
}

/// Starts the service of `engine` and waits until it answers on the studio
/// port. The port must be free; the previous service is stopped first.
///
/// A build shipped without `yue2-service.exe` falls back to the service
/// compiled into this executable - the one case where the studio runs its
/// service in its own process.
fn start_engine_service(engine: &str) -> Result<(), String> {
    if service_is_ready() {
        return Ok(());
    }

    let mut command = match engine_service_command(engine) {
        Ok(command) => command,
        Err(error) if engine == YUE2_ENGINE => {
            eprintln!("{error}; using the service built into this executable");
            return start_service();
        }
        Err(error) => return Err(error),
    };

    let child = command
        .spawn()
        .map_err(|error| format!("could not start the {engine} service: {error}"))?;
    if let Ok(mut running) = ENGINE_SERVICE.lock() {
        if let Some(mut previous) = running.replace(child) {
            let _ = previous.kill();
            let _ = previous.wait();
        }
    }

    if wait_until_ready(Duration::from_secs(45)) {
        Ok(())
    } else {
        Err(format!("the {engine} service did not become ready on 127.0.0.1:{SERVER_PORT}"))
    }
}

/// Stops the service holding the studio port, taking its engine with it.
///
/// The engine lives in a kill-on-close job object of the service's own, so the
/// service dying is the whole of stopping an engine. Nothing can stop a service
/// but its owner, so this only works for the process started here.
fn stop_engine_service() -> Result<(), String> {
    let running = ENGINE_SERVICE.lock().ok().and_then(|mut guard| guard.take());
    let Some(mut child) = running else {
        return Err("the studio service was not started by this window, so it cannot be replaced".into());
    };
    let _ = child.kill();
    let _ = child.wait();
    if wait_until_port_free(Duration::from_secs(15)) {
        Ok(())
    } else {
        Err(format!("port {SERVER_PORT} stayed busy after the studio service was stopped"))
    }
}

/// Switches the studio to the other engine without closing the window.
///
/// The service holding the port is stopped - its engine goes with it - and the
/// service of the chosen engine takes its place. The choice is written first,
/// so a studio restarted at any point in between comes up on the engine the
/// user asked for.
#[tauri::command]
async fn set_engine(app: tauri::AppHandle, engine: String) -> Result<(), String> {
    let engine = if engine == MM3_ENGINE { MM3_ENGINE } else { YUE2_ENGINE }.to_owned();
    // Asking for the engine that is already answering is not a switch: the
    // service on the port is the one the user wants, and restarting it would
    // only cost a reload and whatever the engines had loaded.
    if chosen_engine() == engine && service_is_ready() {
        return Ok(());
    }
    save_engine_choice(&engine)?;

    let can_replace = ENGINE_SERVICE.lock().map(|running| running.is_some()).unwrap_or(false);
    if !can_replace {
        // The studio runs the service compiled into this executable (a build
        // shipped without `yue2-service.exe`), or one started outside the
        // window. Neither can be stopped from here, so the studio restarts.
        app.restart();
    }

    let target = engine.clone();
    tauri::async_runtime::spawn_blocking(move || {
        stop_engine_service()?;
        start_engine_service(&target)
    })
    .await
    .map_err(|error| format!("the engine switch did not finish: {error}"))??;

    // The window's own mark changes with the engine, the way the name in its
    // sidebar does: the taskbar must not keep calling it the other studio.
    apply_engine_icon(&app, &engine);
    remember_engine(&engine);

    // The window stays, its page does not. Everything the interface shows - the
    // form, the tabs, which engine is on the line - is decided by the service,
    // and only a page that asks the new one afresh sees the other engine. The
    // brief flash is the switch.
    tauri::async_runtime::spawn(async move {
        use tauri::Manager as _;
        tokio::time::sleep(Duration::from_millis(250)).await;
        for (_, window) in app.webview_windows() {
            let _ = window.eval("window.location.reload();");
        }
    });
    Ok(())
}

/// Writes the engine the studio should run; `chosen_engine` reads it on start.
fn save_engine_choice(engine: &str) -> Result<(), String> {
    let path = engine_choice_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&path, format!("{{\n  \"engine\": \"{engine}\"\n}}\n"))
        .map_err(|error| format!("could not save the engine choice: {error}"))
}

/// The mark of the engine that is answering, carried inside this executable.
///
/// The icon is the one the window shows in its title bar and the taskbar, and
/// one window over two products has to name the one that is actually running:
/// the mark on both engine bundles is the one their own studio ships.
fn engine_icon(engine: &str) -> Option<tauri::image::Image<'static>> {
    let bytes: &'static [u8] = if engine == MM3_ENGINE {
        include_bytes!("../icons/engine-m3.png")
    } else {
        include_bytes!("../icons/engine-yue2.png")
    };
    tauri::image::Image::from_bytes(bytes).ok()
}

/// Puts the engine's mark on every window of the studio.
fn apply_engine_icon(app: &tauri::AppHandle, engine: &str) {
    use tauri::Manager as _;
    let Some(icon) = engine_icon(engine) else {
        eprintln!("the icon of the {engine} engine could not be read");
        return;
    };
    for (_, window) in app.webview_windows() {
        let _ = window.set_icon(icon.clone());
    }
}

/// Unused in this build: the studio never checks the authors' releases, because
/// installing one would replace it with a single-engine build (see `run`).
#[allow(dead_code)]
fn spawn_update_check(app: tauri::AppHandle, portable: bool) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    use tauri_plugin_updater::UpdaterExt;

    tauri::async_runtime::spawn(async move {
        // The installer is a child of this process, which sits in its own
        // kill-on-close job: without releasing it, the installer died with the
        // studio a moment after starting and the update never happened.
        let cleanup = app.clone();
        // Started from the studio, the installer did not find the previous
        // folder and installed a second copy into its default one. NSIS takes
        // the folder as `/D=`, which must be the last argument and unquoted.
        let install_directory = format!("/D={}", executable_directory().display());
        // The proxy chosen in Settings: GitHub, where updates come from, is
        // among the sites a proxy is set up for.
        let builder = match music_server::net::fixed() {
            music_server::net::Fixed::System => app.updater_builder(),
            music_server::net::Fixed::Direct => app.updater_builder().no_proxy(),
            music_server::net::Fixed::Through(proxy) => app.updater_builder().proxy(proxy),
        };
        let updater = match builder
            .installer_arg(install_directory)
            .on_before_exit(move || {
                cleanup.cleanup_before_exit();
                if !music_engine::process_group::release_children() {
                    eprintln!("[ERROR] could not release the update installer from the studio's job");
                }
            })
            .build()
        {
            Ok(updater) => updater,
            Err(_) => return,
        };
        let update = match updater.check().await {
            Ok(Some(update)) => update,
            _ => return,
        };

        let version = update.version.clone();
        if portable {
            let open_release = app
                .dialog()
                .message(format!("YuE2 Studio {version} is available. Open the download page?"))
                .title("YuE2 Studio update")
                .kind(MessageDialogKind::Info)
                .buttons(MessageDialogButtons::OkCancelCustom("Open".into(), "Later".into()))
                .blocking_show();
            if open_release {
                use tauri_plugin_opener::OpenerExt;
                let _ = app.opener().open_url(RELEASES_URL, None::<&str>);
            }
            return;
        }

        let install = app
            .dialog()
            .message(format!("YuE2 Studio {version} is available. Install it now?"))
            .title("YuE2 Studio update")
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom("Install".into(), "Later".into()))
            .blocking_show();
        if !install {
            return;
        }

        if let Err(error) = update.download_and_install(|_, _| {}, || {}).await {
            app.dialog()
                .message(format!("Could not install the update: {error}"))
                .title("YuE2 Studio update")
                .kind(MessageDialogKind::Error)
                .blocking_show();
            return;
        }
        app.restart();
    });
}

#[cfg(windows)]
fn hide_own_console_window() {
    unsafe extern "system" {
        fn GetConsoleWindow() -> isize;
        fn GetConsoleProcessList(processes: *mut u32, count: u32) -> u32;
        fn ShowWindow(window: isize, command: i32) -> i32;
    }

    unsafe {
        let mut processes = [0_u32; 4];
        if GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) == 1 {
            let window = GetConsoleWindow();
            if window != 0 {
                ShowWindow(window, 0);
            }
        }
    }
}

/// Tauri's own WebView2 switches, followed by whatever the user set in the
/// variable WebView2 documents for exactly this.
#[cfg(windows)]
fn webview_browser_arguments() -> String {
    // The visualiser's own window hears the studio through an audio context
    // of its own, which nothing in that window has clicked on to start.
    let mut own = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required".to_owned();
    // "No proxy" for what the window loads itself; a proxy of the user's own
    // is given through Tauri's proxy_url where it takes the scheme.
    let proxy = music_server::saved_proxy();
    if proxy.mode == music_server::net::ProxyMode::Off {
        own.push_str(" --no-proxy-server");
    } else if let Some(url) = proxy.window_proxy().filter(|url| !matches!(url.scheme(), "http" | "socks5")) {
        own.push_str(&format!(" --proxy-server={}://{}:{}", url.scheme(), url.host_str().unwrap_or_default(), url.port_or_known_default().unwrap_or_default()));
    }
    match std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS") {
        Ok(extra) if !extra.trim().is_empty() => format!("{own} {}", extra.trim()),
        _ => own,
    }
}

/// The proxy a window is given through Tauri's proxy_url, which takes HTTP and
/// SOCKS5; the other schemes go in the browser arguments. Every window takes
/// the same, or WebView2 refuses the second one.
fn window_proxy_url() -> Option<reqwest::Url> {
    music_server::saved_proxy().window_proxy().filter(|url| matches!(url.scheme(), "http" | "socks5"))
}

/// The visualiser in a window of its own, or the one already open brought
/// forward. Built here rather than from the page: every window of the app
/// shares one WebView2 environment, which must be opened with the same
/// switches the main window was, or the new window fails to open. Async:
/// on Windows a window made from a synchronous command deadlocks WebView2.
#[tauri::command]
async fn open_visualizer_window(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager as _;
    if let Some(window) = app.get_webview_window("visualizer") {
        window.show().map_err(|error| error.to_string())?;
        return window.set_focus().map_err(|error| error.to_string());
    }
    let title = format!("{} - Visualizer", app.package_info().name);
    let window = tauri::WebviewWindowBuilder::new(&app, "visualizer", tauri::WebviewUrl::App("visualizer.html".into()))
        .title(title)
        .inner_size(960.0, 540.0)
        .min_inner_size(320.0, 200.0);
    #[cfg(windows)]
    let window = window.additional_browser_args(&webview_browser_arguments());
    let window = match window_proxy_url() {
        Some(url) => window.proxy_url(url),
        None => window,
    };
    window.build().map(|_| ()).map_err(|error| error.to_string())
}

/// The window's shape: only these rectangles, [left, top, right, bottom] in
/// physical pixels of the page, are the window, and a click anywhere else
/// goes to whatever lies below. No rectangles: the whole window again. The
/// Winamp mode spreads the window over the screen and cuts it to Winamp's
/// own windows, so they can be moved apart, docked and resized like Winamp's.
#[tauri::command]
fn set_window_region(window: tauri::WebviewWindow, rects: Vec<[i32; 4]>) -> Result<(), String> {
    #[cfg(windows)]
    {
        #[link(name = "gdi32")]
        unsafe extern "system" {
            fn CreateRectRgn(left: i32, top: i32, right: i32, bottom: i32) -> isize;
            fn CombineRgn(destination: isize, first: isize, second: isize, mode: i32) -> i32;
            fn DeleteObject(object: isize) -> i32;
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn SetWindowRgn(window: isize, region: isize, redraw: i32) -> i32;
            fn GetWindowRect(window: isize, rect: *mut [i32; 4]) -> i32;
            fn ClientToScreen(window: isize, point: *mut [i32; 2]) -> i32;
        }
        const RGN_OR: i32 = 2;
        let hwnd = window.hwnd().map_err(|error| error.to_string())?.0 as isize;
        unsafe {
            let region = if rects.is_empty() {
                0
            } else {
                // a region counts from the window's outer corner, which a frameless window
                // still keeps a few invisible border pixels away from its page
                let mut outer = [0i32; 4];
                let mut page = [0i32; 2];
                if GetWindowRect(hwnd, &mut outer) == 0 || ClientToScreen(hwnd, &mut page) == 0 {
                    return Err("Windows did not tell where the window's page is".into());
                }
                let (dx, dy) = (page[0] - outer[0], page[1] - outer[1]);
                let region = CreateRectRgn(0, 0, 0, 0);
                if region == 0 {
                    return Err("Windows made no region for the window".into());
                }
                // Chromium apps take a window whose region is one plain rectangle for an opaque
                // one of its whole size (IsWindowVisibleAndFullyOpaque) and stop drawing beneath
                // it: two lone pixels in the invisible border keep the shape never that plain
                let stubs = [[-dx, -dy, 1 - dx, 1 - dy], [2 - dx, -dy, 3 - dx, 1 - dy]];
                for [left, top, right, bottom] in stubs.into_iter().chain(rects) {
                    let part = CreateRectRgn(left + dx, top + dy, right + dx, bottom + dy);
                    if part == 0 || CombineRgn(region, region, part, RGN_OR) == 0 {
                        DeleteObject(region);
                        if part != 0 {
                            DeleteObject(part);
                        }
                        return Err("Windows could not build the window's shape".into());
                    }
                    DeleteObject(part);
                }
                region
            };
            // the window owns the region from here on
            if SetWindowRgn(hwnd, region, 1) == 0 {
                if region != 0 {
                    DeleteObject(region);
                }
                return Err("Windows did not take the window's shape".into());
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (window, rects);
    Ok(())
}

pub fn run() {
    #[cfg(windows)]
    hide_own_console_window();

    if keeps_everything_beside_itself() && std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none() {
        // The process is still single-threaded here, before Tauri or its
        // worker threads are created, so updating its child WebView environment
        // cannot race with an environment read.
        unsafe {
            std::env::set_var(
                "WEBVIEW2_USER_DATA_FOLDER",
                executable_directory().join("webview-data"),
            );
        }
    }

    // The native engine is started only by the setup gate after a complete
    // verified profile is present. This keeps first launch download-free and
    // avoids starting a runtime against partial weights.
    // Whatever ends this process - the window, Task Manager, a crash - takes
    // the engine with it. Without this the engine outlived a force-killed
    // studio, holding the graphics card and the port it listens on.
    if !music_engine::process_group::bind_children_to_this_process() {
        eprintln!("this process could not create its own job object; the engine is stopped by the supervisor only");
    }

    configure_studio_runtime_paths();

    // the service names the studio's version to agents, which only the shell knows
    let context = tauri::generate_context!();
    music_server::set_studio_version(context.package_info().version.to_string());

    // One start is one session: the engine logs of the session before are let
    // go of here and nowhere else, so switching engines keeps both logs.
    clear_engine_logs(&studio_data_directory());

    // A start remembers the engine it runs, and writes the default down when
    // nothing was ever chosen: closing the studio hands the same engine to the
    // start after it.
    let engine = chosen_engine();
    remember_engine(&engine);
    if !engine_choice_path().is_file() {
        if let Err(error) = save_engine_choice(&engine) {
            eprintln!("could not remember the engine: {error}");
        }
    }

    // The chosen engine decides which service answers on this port. Both are
    // processes of their own beside the executable, so the switch can stop one
    // and start the other without the window being closed.
    if let Err(error) = start_engine_service(&engine) {
        eprintln!("failed to start the studio service: {error}");
    }

    // The updater is deliberately not configured in this build. It points at
    // the authors' releases, and installing one would replace this studio with
    // a single-engine build of theirs - the hybrid would overwrite itself. The
    // second engine and its models are updated by the studio, not by them.
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![open_visualizer_window, set_window_region, set_engine]);

    builder
        .setup(move |app| {
            // "Save as" for what the window saves: Windows' own dialog, over
            // the studio's window, asked for by the service off the UI thread.
            let dialogs = app.handle().clone();
            music_server::set_save_dialog(Box::new(move |name: &str, start: Option<std::path::PathBuf>| {
                use tauri::Manager;
                use tauri_plugin_dialog::DialogExt;
                let mut dialog = dialogs.dialog().file().set_file_name(name);
                if let Some(extension) = std::path::Path::new(name).extension().and_then(|extension| extension.to_str()) {
                    dialog = dialog.add_filter(extension.to_uppercase(), &[extension]);
                }
                if let Some(folder) = start {
                    dialog = dialog.set_directory(folder);
                }
                if let Some(window) = dialogs.get_webview_window("main") {
                    dialog = dialog.set_parent(&window);
                }
                dialog.blocking_save_file().and_then(|path| path.into_path().ok())
            }));
            // The window is built here rather than from the configuration so
            // the WebView2 arguments can be extended: Tauri passes its own, and
            // WebView2 then ignores WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
            // entirely, so a remote-debugging port or any other documented
            // switch set by the user would silently never arrive.
            let window_config = app
                .config()
                .app
                .windows
                .iter()
                .find(|window| window.label == "main")
                .cloned()
                .expect("the main window is declared in tauri.conf.json");
            let window = tauri::WebviewWindowBuilder::from_config(app.handle(), &window_config)?;
            #[cfg(windows)]
            let window = window.additional_browser_args(&webview_browser_arguments());
            // The proxy chosen in Settings for what the window loads itself -
            // the image and video searches of the video maker; the service
            // routes its own requests.
            let window = match window_proxy_url() {
                Some(url) => window.proxy_url(url),
                None => window,
            };

            let window = window.build()?;
            // The mark of the engine this start runs: the title bar and the
            // taskbar name the same studio the page does.
            if let Some(icon) = engine_icon(&chosen_engine()) {
                let _ = window.set_icon(icon);
            }
            // The webview starts loading the moment the window exists, and the
            // service can finish binding a fraction of a second later. That is
            // enough for the browser to keep its own connection error on screen
            // for good - checking readiness once here is not enough, because the
            // race is already lost by then. So the page is reloaded whenever it
            // is still empty, for the first half minute of the session.
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    use tauri::Manager as _;
                    if !wait_until_ready(Duration::from_secs(90)) {
                        return;
                    }
                    let started = Instant::now();
                    while started.elapsed() < Duration::from_secs(30) {
                        std::thread::sleep(Duration::from_millis(700));
                        for (_, window) in handle.webview_windows() {
                            // Reload only while nothing has rendered: reloading
                            // a working studio would throw away what the user
                            // already has on screen.
                            let _ = window.eval(
                                "if (!document.getElementById('root') || !document.getElementById('root').firstElementChild) { window.location.reload(); }",
                            );
                        }
                    }
                });
            }
            Ok(())
        })
        .build(context)
        .expect("error while building Ai Music Studio")
        .run(|_handle, event| {
            // Closing writes down what the window was running, so the next
            // start opens on the same engine.
            if let tauri::RunEvent::Exit = event {
                if let Some(engine) = current_engine() {
                    if let Err(error) = save_engine_choice(&engine) {
                        eprintln!("could not remember the engine on exit: {error}");
                    }
                }
            }
        });
}

