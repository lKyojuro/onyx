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
const MAX_RAYON_THREADS: usize = 4;

enum Launch {
    Version,
    Smoke,
    Window(Option<PathBuf>),
}

fn launch_mode() -> AppResult<Launch> {
    let mut args = std::env::args_os().skip(1);
    let Some(arg) = args.next() else {
        return Ok(Launch::Window(None));
    };
    let mode = match arg.to_str() {
        Some("--version" | "-V") => Launch::Version,
        Some("--smoke-test") => Launch::Smoke,
        Some("--") => Launch::Window(Some(args.next().ok_or("missing path after --")?.into())),
        Some(flag) if flag.starts_with('-') => return Err(format!("unknown option: {flag}").into()),
        _ => Launch::Window(Some(arg.into())),
    };
    if args.next().is_some() {
        return Err("usage: onyx-ui-gpui [PATH] | --version | --smoke-test".into());
    }
    Ok(mode)
}

fn default_initial_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit
        && path.is_dir()
    {
        return path;
    }
    let onyx_dir = PathBuf::from(r"D:\Onyx");
    if onyx_dir.is_dir() {
        return onyx_dir;
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

fn main() -> AppResult<()> {
    let mode = launch_mode()?;
    if matches!(mode, Launch::Version) {
        println!("onyx-ui-gpui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(MAX_RAYON_THREADS)
        .build_global();

    let runtime = Arc::new(
        Builder::new_multi_thread()
            .worker_threads(async_worker_threads())
            .max_blocking_threads(MAX_BLOCKING_THREADS)
            .enable_all()
            .build()?,
    );

    match mode {
        Launch::Version => Ok(()),
        Launch::Smoke => runtime.block_on(smoke_test()),
        Launch::Window(explicit_path) => {
            let initial_path = default_initial_path(explicit_path);
            run_window(runtime, initial_path);
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

fn run_window(runtime: Arc<Runtime>, initial_path: PathBuf) {
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
