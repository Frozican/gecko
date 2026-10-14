use std::sync::{Arc, Mutex};

use backend_wgpu::GxRenderer;
use gecko::HostInput;
use gecko::gamecube::GameCube;
use gecko::wii::Wii;
use image::Dol;

use crate::sink::{Introspection, McpSink};
use crate::state::Backend;

pub struct BootResult {
    pub backend: Backend,
    pub game_name: String,
    pub game_code: String,
}

fn make_sink(
    gx: Arc<Mutex<GxRenderer>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    introspect: Arc<Mutex<Introspection>>,
) -> Box<McpSink> {
    Box::new(McpSink {
        gx,
        device,
        queue,
        introspect,
        scratch: Vec::new(),
    })
}

fn finalize_wii(mut emu: Wii, dsp_rom: Option<&[u8]>, coef_rom: Option<&[u8]>, sink: Box<McpSink>) -> Wii {
    if let Some(rom) = dsp_rom {
        emu.dsp.load_irom(rom);
    }
    if let Some(rom) = coef_rom {
        emu.dsp.load_coef(rom);
    }
    emu.render_sink = sink;
    emu.apply_host_input(&HostInput::wii_neutral());
    emu
}

fn finalize_gc(mut emu: GameCube, dsp_rom: Option<&[u8]>, coef_rom: Option<&[u8]>, sink: Box<McpSink>) -> GameCube {
    if let Some(rom) = dsp_rom {
        emu.dsp.load_irom(rom);
    }
    if let Some(rom) = coef_rom {
        emu.dsp.load_coef(rom);
    }
    emu.render_sink = sink;
    emu.apply_host_input(&HostInput::gc_connected());
    emu
}

pub fn boot(
    disc_bytes: Vec<u8>,
    ipl: Option<&[u8]>,
    dsp_rom: Option<&[u8]>,
    coef_rom: Option<&[u8]>,
    gx: Arc<Mutex<GxRenderer>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    introspect: Arc<Mutex<Introspection>>,
) -> BootResult {
    let dvd = image::load_dvd(disc_bytes);

    let game_name = String::from_utf8_lossy(&dvd.header().game_name)
        .trim_end_matches('\0')
        .to_owned();
    let game_code = String::from_utf8_lossy(&dvd.header().game_code)
        .trim_end_matches('\0')
        .to_owned();

    let sink = make_sink(gx, device, queue, introspect);

    let backend = if dvd.header().is_wii() {
        Backend::Wii(finalize_wii(Wii::apploader_hle(dvd).build(), dsp_rom, coef_rom, sink))
    } else {
        let emu = match ipl {
            Some(ipl) => {
                let mut emu = GameCube::with_ipl(ipl, true);
                emu.insert_dvd(dvd);
                emu
            }
            None => GameCube::with_ipl_hle(dvd),
        };
        Backend::Gc(finalize_gc(emu, dsp_rom, coef_rom, sink))
    };

    BootResult {
        backend,
        game_name,
        game_code,
    }
}

pub fn boot_dol(
    dol_bytes: Vec<u8>,
    wii: bool,
    dsp_rom: Option<&[u8]>,
    coef_rom: Option<&[u8]>,
    gx: Arc<Mutex<GxRenderer>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    introspect: Arc<Mutex<Introspection>>,
) -> BootResult {
    let dol = Dol::parse(dol_bytes);
    let sink = make_sink(gx, device, queue, introspect);

    let backend = if wii {
        Backend::Wii(finalize_wii(Wii::with_image(&dol), dsp_rom, coef_rom, sink))
    } else {
        Backend::Gc(finalize_gc(GameCube::with_image(&dol), dsp_rom, coef_rom, sink))
    };

    BootResult {
        backend,
        game_name: String::new(),
        game_code: String::new(),
    }
}
