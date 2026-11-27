#![forbid(unsafe_code)]
#![cfg_attr(windows, windows_subsystem = "windows")]

mod components;
mod explorer;
mod icons;
mod led_pill;
mod search_index;
mod theme;

use explorer::OnyxExplorer;
use gpui_kit::{
    AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, assets::Assets, px, size,
};
use onyx_core::GenerationId;
use onyx_fs::{EnumerationOptions, FsEngine};
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const MIN_ASYNC_WORKERS: usize = 2;
const MAX_ASYNC_WORKERS: usize = 4;
const MAX_BLOCKING_THREADS: usize = 32;

/// Parameters of a window launch, after single-instance forwarding was skipped.
#[derive(Debug, Default)]
struct WindowLaunch {
    /// Path given on the command line, forwarded to the running instance.
    explicit_path: Option<PathBuf>,
    /// Open a window even when an instance already listens on the IPC pipe.
    new_window: bool,
    /// Start warm in the background without stealing the foreground.
    background: bool,
}

#[derive(Debug)]
enum Launch {
    Version,
    Smoke,
    Window(WindowLaunch),
}

fn launch_mode() -> AppResult<Launch> {
    let mut launch = WindowLaunch::default();
    let mut args = std::env::args_os().skip(1);
    let mut mode = None;
    while let Some(arg) = args.next() {
        let Some(flag) = arg.to_str() else {
            set_explicit_path(&mut launch, arg.into())?;
            continue;
        };
        match flag {
            "--version" | "-V" => mode = Some(Launch::Version),
            "--smoke-test" => mode = Some(Launch::Smoke),
            "--new-window" => launch.new_window = true,
            "--background" => launch.background = true,
            "--" => {
                let path = args.next().ok_or("missing path after --")?;
                set_explicit_path(&mut launch, path.into())?;
            }
            _ if flag.starts_with('-') => return Err(format!("unknown option: {flag}").into()),
            _ => set_explicit_path(&mut launch, arg.into())?,
        }
    }

    if let Some(mode) = mode {
        if launch.explicit_path.is_some() || launch.new_window || launch.background {
            return Err("--version and --smoke-test do not take a path or window flags".into());
        }
        return Ok(mode);
    }
    Ok(Launch::Window(launch))
}

fn set_explicit_path(launch: &mut WindowLaunch, path: PathBuf) -> AppResult<()> {
    if launch.explicit_path.replace(path).is_some() {
        return Err("only one path can be opened per launch".into());
    }
    Ok(())
}

fn default_initial_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit
        && path.is_dir()
    {
        return path;
    }
    dirs::home_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from(r"C:\"))
}

fn async_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(MIN_ASYNC_WORKERS, |n| n.get())
        .clamp(MIN_ASYNC_WORKERS, MAX_ASYNC_WORKERS)
}

fn build_runtime() -> AppResult<Arc<Runtime>> {
    Ok(Arc::new(
        Builder::new_multi_thread()
            .worker_threads(async_worker_threads())
            .max_blocking_threads(MAX_BLOCKING_THREADS)
            .enable_all()
            .build()?,
    ))
}

fn main() -> AppResult<()> {
    onyx_windows::attach_parent_console();
    let mode = launch_mode()?;

    match mode {
        Launch::Version => {
            println!("onyx {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Launch::Smoke => build_runtime()?.block_on(smoke_test()),
        Launch::Window(launch) => {
            // Warm re-launch: a running Onyx takes the path and this process exits
            // before the Tokio runtime or the GPUI application exists, so a second
            // launch costs one `CreateFileW` on the IPC pipe.
            if !launch.new_window
                && onyx_windows::try_send_to_existing_instance(launch.explicit_path.as_deref())
            {
                return Ok(());
            }
            let runtime = build_runtime()?;
            let initial_path = default_initial_path(launch.explicit_path);
            run_window(runtime, initial_path, launch.background);
            Ok(())
        }
    }
}

async fn smoke_test() -> AppResult<()> {
    let dir = std::env::current_dir().unwrap_or_else(|_| default_initial_path(None));
    let cancel = CancellationToken::new();
    let entries = FsEngine::new()
        .enumerate_dir(
            dir.clone(),
            GenerationId::new(1),
            EnumerationOptions::default(),
            cancel,
        )
        .await?;
    println!(
        "smoke-test: ok | backend=gpui-fast path={} entries={}",
        dir.display(),
        entries.len()
    );
    Ok(())
}

fn run_window(runtime: Arc<Runtime>, initial_path: PathBuf, background: bool) {
    gpui_kit::application().with_assets(Assets).run(move |cx| {
        gpui_kit::init(cx);

        let bounds = Bounds::centered(None, size(px(1320.0), px(820.0)), cx);
        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(960.0), px(620.0))),
            titlebar: Some(TitlebarOptions {
                title: Some("Onyx".into()),
                appears_transparent: true,
                traffic_light_position: None,
            }),
            app_owns_titlebar_drag: true,
            // `--background` starts warm without taking the foreground: the
            // window is drawn, the IPC listener answers the next launch, and the
            // user's current window keeps focus.
            focus: !background,
            ..WindowOptions::default()
        };

        let runtime = Arc::clone(&runtime);
        let initial_path = initial_path.clone();
        if let Err(err) = gpui_kit::open_window(window_options, cx, move |window, cx| {
            cx.new(|cx| OnyxExplorer::new(runtime, initial_path, window, cx))
        }) {
            eprintln!("failed to open Onyx GPUI window: {err}");
            cx.quit();
        }
    });
}
