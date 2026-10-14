use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result};
use backend_wgpu::GxRenderer;
use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

use mcp::server::McpServer;
use mcp::sink::Introspection;
use mcp::state::{EmuState, RunMode, Shared};
use mcp::{boot, worker};

#[derive(Parser, Debug)]
#[command(name = "mcp", about = "MCP server exposing the gecko GameCube/Wii emulator to LLMs")]
struct Cli {
    #[arg(
        long,
        value_name = "PATH",
        help = "Disc image to load on startup (ISO/RVZ/ZIP)",
        conflicts_with = "dol"
    )]
    dvd: Option<PathBuf>,

    #[arg(long, value_name = "PATH", help = "DOL homebrew to load on startup")]
    dol: Option<PathBuf>,

    #[arg(
        long,
        help = "Boot the DOL as a Wii executable instead of GameCube",
        requires = "dol"
    )]
    wii: bool,

    #[arg(
        long,
        value_name = "PATH",
        help = "Decoded GameCube IPL to boot discs with instead of IPL HLE"
    )]
    ipl: Option<PathBuf>,

    #[arg(long, value_name = "PATH", help = "DSP IROM to use instead of Dolphin's DSP ROM")]
    dsp: Option<PathBuf>,

    #[arg(long, value_name = "PATH", help = "DSP COEF ROM to use instead of Dolphin's DSP ROM")]
    coef: Option<PathBuf>,

    #[arg(long, help = "Resume the emulator immediately after loading the disc")]
    resume: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();

    let ipl = cli
        .ipl
        .map(|p| std::fs::read(&p).with_context(|| format!("read IPL {p:?}")))
        .transpose()?;

    let dsp_rom = cli
        .dsp
        .map(|p| std::fs::read(&p).with_context(|| format!("read DSP IROM {p:?}")))
        .transpose()?;

    let coef_rom = cli
        .coef
        .map(|p| std::fs::read(&p).with_context(|| format!("read DSP COEF ROM {p:?}")))
        .transpose()?;

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .context("no compatible wgpu adapter")?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .context("acquire wgpu device")?;
    let surface_format = wgpu::TextureFormat::Rgba8Unorm;

    let gx = Arc::new(Mutex::new(GxRenderer::new(&device, &queue, surface_format, 1)));
    let introspect = Arc::new(Mutex::new(Introspection::default()));

    let shared = Arc::new(Shared {
        state: Mutex::new(EmuState::new()),
        cv: Condvar::new(),
        gx: gx.clone(),
        device: device.clone(),
        queue: queue.clone(),
        introspect: introspect.clone(),
        ipl,
        dsp_rom,
        coef_rom,
    });

    let initial = if let Some(dol_path) = cli.dol.as_ref() {
        let bytes = std::fs::read(dol_path).with_context(|| format!("read DOL {dol_path:?}"))?;
        Some(boot::boot_dol(
            bytes,
            cli.wii,
            shared.dsp_rom.as_deref(),
            shared.coef_rom.as_deref(),
            shared.gx.clone(),
            shared.device.clone(),
            shared.queue.clone(),
            shared.introspect.clone(),
        ))
    } else if let Some(dvd_path) = cli.dvd.as_ref() {
        let bytes = std::fs::read(dvd_path).with_context(|| format!("read DVD {dvd_path:?}"))?;
        Some(boot::boot(
            bytes,
            shared.ipl.as_deref(),
            shared.dsp_rom.as_deref(),
            shared.coef_rom.as_deref(),
            shared.gx.clone(),
            shared.device.clone(),
            shared.queue.clone(),
            shared.introspect.clone(),
        ))
    } else {
        None
    };

    if let Some(result) = initial {
        let mut s = shared.state.lock().unwrap();
        s.backend = Some(result.backend);
        s.game_name = result.game_name;
        s.game_code = result.game_code;
        s.run_mode = if cli.resume { RunMode::Running } else { RunMode::Paused };
        drop(s);

        shared.cv.notify_all();
    }

    let worker_shared = shared.clone();
    std::thread::Builder::new()
        .name("mcp-emu".into())
        .spawn(move || worker::run(worker_shared))
        .context("spawn emu worker")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    runtime.block_on(async move {
        let server = McpServer::new(shared);
        let service = server.serve(stdio()).await.context("rmcp serve")?;
        service.waiting().await.context("rmcp waiting")?;
        anyhow::Ok(())
    })?;

    Ok(())
}
