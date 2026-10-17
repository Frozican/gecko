pub mod regs;

use crate::async_disc::{AsyncDiscPort, DiscReadRequest, DiscReadResponse};
use crate::system::{System, SystemId};

pub enum Command {
    DriveInfo,
    ReadDiskId,
    ReadSectorData,
    AudioToggle(bool),
}

pub struct DvdInterface {
    pub status: regs::DiStatusRegister,
    pub cover: regs::DiCoverRegister,
    pub dma_address: regs::DiDmaAddressRegister,
    pub dma_length: regs::DiDmaLengthRegister,
    pub control: regs::DiControlRegister,
    pub config: regs::DiConfigurationRegister,
    pub cmdbuf0: u32,
    pub cmdbuf1: u32,
    pub cmdbuf2: u32,
    pub immbuf: u32,
    pub dvd: Option<Box<dyn image::Dvd>>,
    /// When enabled, data commands are completed by the host through AsyncDiscPort.
    pub async_reads_enabled: bool,
    pub async_port: AsyncDiscPort,
}

impl DvdInterface {
    pub fn new() -> Self {
        Self {
            status: regs::DiStatusRegister::from_raw(0),
            cover: regs::DiCoverRegister::from_raw(0),
            dma_address: regs::DiDmaAddressRegister::from_raw(0),
            dma_length: regs::DiDmaLengthRegister::from_raw(0),
            control: regs::DiControlRegister::from_raw(0),
            config: regs::DiConfigurationRegister::default(),
            cmdbuf0: 0,
            cmdbuf1: 0,
            cmdbuf2: 0,
            immbuf: 0,
            dvd: None,
            async_reads_enabled: false,
            async_port: AsyncDiscPort::new(),
        }
    }

    #[inline(always)]
    pub fn interrupt_active(&self) -> bool {
        (self.status.break_complete() && self.status.break_complete_mask())
            || (self.status.device_error() && self.status.device_error_mask())
            || (self.status.transfer_complete() && self.status.transfer_complete_mask())
            || (self.cover.cover_interrupt() && self.cover.cover_interrupt_mask())
    }

    #[inline(always)]
    fn resolve_command(&self) -> Option<Command> {
        let cmd = self.cmdbuf0 >> 24;
        let sub1 = (self.cmdbuf0 >> 16) & 0xFF;
        let sub2 = self.cmdbuf0 & 0xFFFF;

        match (cmd, sub1, sub2) {
            (0x12, 0x00, 0x0000) => Some(Command::DriveInfo),
            (0xA8, _, 0x0000) => Some(Command::ReadSectorData),
            (0xA8, _, 0x0040) => Some(Command::ReadDiskId),
            (0xE4, 0x00, _) => Some(Command::AudioToggle(false)),
            (0xE4, 0x01, _) => Some(Command::AudioToggle(true)),
            _ => {
                tracing::error!(
                    cmd = format!("{cmd:02X}"),
                    sub1 = format!("{sub1:02X}"),
                    sub2 = format!("{sub2:04X}"),
                    cmdbuf1 = format!("{:08X}", self.cmdbuf1),
                    cmdbuf2 = format!("{:08X}", self.cmdbuf2),
                    "Unknown DI command"
                );
                None
            }
        }
    }
}

crate::mmio_device_dispatch! {
    read = di_read,
    write = di_write,
    registers = [
        regs::DiStatusRegister,
        regs::DiCoverRegister,
        regs::DiCommandBuf0,
        regs::DiCommandBuf1,
        regs::DiCommandBuf2,
        regs::DiDmaAddressRegister,
        regs::DiDmaLengthRegister,
        regs::DiControlRegister,
        regs::DiImmBuf,
        regs::DiConfigurationRegister,
    ],
}

#[inline(always)]
pub fn refresh_interrupts<const SYSTEM: SystemId>(sys: &mut System<SYSTEM>) {
    use crate::flipper::pi::InterruptFlag;

    if sys.di.interrupt_active() {
        sys.pi.assert_interrupt(InterruptFlag::Di);
    } else {
        sys.pi.clear_interrupt(InterruptFlag::Di);
    }
}

pub fn start_transfer<const SYSTEM: SystemId>(sys: &mut System<SYSTEM>) {
    let Some(dvd) = sys.di.dvd.take() else {
        // TODO: Setting this causes unrecoverable error when there is no DVD
        //sys.di.status.set_device_error(true);
        sys.di.control.set_tstart(false);
        return;
    };

    let mut waiting_for_async_read = false;
    if let Some(cmd) = sys.di.resolve_command() {
        if sys.di.async_reads_enabled {
            if let Some((offset, length, destination)) = async_read_parameters(&sys.di, &cmd) {
                match sys.di.async_port.begin(offset, length, destination) {
                    Ok(request) => {
                        tracing::debug!(
                            id = request.id,
                            offset = format!("{:X}", request.offset),
                            length = request.length,
                            destination = format!("{:08X}", request.destination),
                            "Queued asynchronous DVD read"
                        );
                        waiting_for_async_read = true;
                    }
                    Err(error) => {
                        tracing::error!(%error, "Could not queue asynchronous DVD read");
                        sys.di.status.set_device_error(true);
                        sys.di.control.set_tstart(false);
                        self::refresh_interrupts(sys);
                    }
                }
            } else {
                self::process_dvd_command(sys, cmd, &dvd);
            }
        } else {
            self::process_dvd_command(sys, cmd, &dvd);
        }
    }

    sys.di.dvd = Some(dvd);

    // The host will complete this transfer after its asynchronous File read.
    if waiting_for_async_read || sys.di.async_port.pending().is_some() {
        return;
    }

    // The interrupt must not fire until some time!!
    // This is important as it will break the IPL if we immediately raise it...
    // When DVDReadDiskID issues the DI command, it returns to BS2_Tick which updates
    // the internal state machine and then calls into restore_irq. The code assumes
    // that the interrupt wont fire until after it has returned and updated the state.
    // Else it will get trapped inside restore_irq, right after mtmsr, executing in a loop
    // of the DVD dispatch handler which in turn re-issues the same command...
    const DI_TRANSFER_DELAY_US: u64 = 20; // Based off of vxpm and hazel (~10k cycles at GC clock)
    sys.scheduler.schedule_in(
        crate::scheduler::microseconds_to_cycles(SYSTEM, DI_TRANSFER_DELAY_US),
        crate::scheduler::Handler::DiTransferDone,
    );
}

pub fn transfer_done_handler<const SYSTEM: SystemId>(sys: &mut System<SYSTEM>) {
    sys.di.control.set_tstart(false);
    // DMA length tracks the progress of the transfer, so when it hits 0, the
    // transfer is complete. On failure, this would denote how many bytes were
    // not transferred, but we close our eyes and just hope nothing depends on
    // that!
    sys.di.dma_length = regs::DiDmaLengthRegister::from_raw(0);
    sys.di.status.set_transfer_complete(true);
    self::refresh_interrupts(sys);
}

#[inline(always)]
fn async_read_parameters(di: &DvdInterface, cmd: &Command) -> Option<(u64, usize, u32)> {
    let destination = di.dma_address.address();
    match cmd {
        Command::ReadSectorData => {
            let offset = (di.cmdbuf1 as u64) << 2;
            let length = di.cmdbuf2 as usize;
            assert!(length == di.dma_length.length() as usize, "DMA length mismatch");
            Some((offset, length, destination))
        }
        Command::ReadDiskId => {
            let length = di.dma_length.length() as usize;
            Some((0, length, destination))
        }
        _ => None,
    }
}

impl<const SYSTEM: SystemId> System<SYSTEM> {
    /// Enable host-serviced asynchronous reads for DVD data commands.
    /// Boot/apploader reads still use the synchronous image API.
    pub fn enable_async_dvd_reads(&mut self, enabled: bool) {
        self.di.async_reads_enabled = enabled;
    }

    /// Obtain the read that the browser host must fulfill, if any.
    pub fn pending_async_dvd_read(&self) -> Option<DiscReadRequest> {
        self.di.async_port.pending()
    }

    /// Finish a host-serviced read, copy it into emulated RAM, and schedule DI completion.
    pub fn complete_async_dvd_read(
        &mut self,
        response: DiscReadResponse,
    ) -> Result<(), String> {
        let request = self
            .di
            .async_port
            .pending()
            .ok_or_else(|| "no asynchronous DVD read is pending".to_owned())?;

        match self.di.async_port.complete(response) {
            Ok(bytes) => {
                let destination = request.destination;
                let length = request.length;
                self.mmio
                    .phys_slice_mut(destination, length)
                    .copy_from_slice(&bytes);
                #[cfg(feature = "jit")]
                self.mmio.queue_icbi_for_range(destination, length as u32);
                self.scheduler.schedule_in(
                    crate::scheduler::microseconds_to_cycles(SYSTEM, 20),
                    crate::scheduler::Handler::DiTransferDone,
                );
                Ok(())
            }
            Err(error) => {
                self.di.status.set_device_error(true);
                self.di.control.set_tstart(false);
                self::refresh_interrupts(self);
                Err(error)
            }
        }
    }
}

fn process_dvd_command<const SYSTEM: SystemId>(sys: &mut System<SYSTEM>, cmd: Command, dvd: &dyn image::Dvd) {
    match cmd {
        Command::DriveInfo => {
            let dst = sys.di.dma_address.address();
            let buffer = sys.mmio.phys_slice_mut(dst, 0x20);
            buffer.copy_from_slice(&[0x69; 0x20]); // TODO: Drive Info?
            #[cfg(feature = "jit")]
            sys.mmio.queue_icbi_for_range(dst, 0x20);
        }
        Command::ReadSectorData => {
            let src = sys.di.cmdbuf1 << 2;
            let dst = sys.di.dma_address.address();
            let len = sys.di.cmdbuf2 as usize;
            assert!(len == sys.di.dma_length.length() as usize, "DMA length mismatch");

            let buffer = sys.mmio.phys_slice_mut(dst, len);
            dvd.read_disc_into(src as usize, buffer);
            #[cfg(feature = "jit")]
            sys.mmio.queue_icbi_for_range(dst, len as u32);

            tracing::debug!(
                src = format!("{:08X}", src),
                dst = format!("{:08X}", dst),
                len = format!("{:08X}", len),
                "ReadSectorData command"
            );
        }
        Command::ReadDiskId => {
            let src = sys.di.cmdbuf1;
            let dst = sys.di.dma_address.address();
            let len = sys.di.dma_length.length() as usize;

            let buffer = sys.mmio.phys_slice_mut(dst, len);
            dvd.read_disc_into(0, buffer);
            #[cfg(feature = "jit")]
            sys.mmio.queue_icbi_for_range(dst, len as u32);

            tracing::debug!(
                src = format!("{:08X}", src),
                dst = format!("{:08X}", dst),
                len = format!("{:08X}", len),
                "ReadDiskId command"
            );
        }
        Command::AudioToggle(enable) => {
            tracing::warn!(enable, "AudioToggle stubbed");
            sys.di.immbuf = 0;
        }
    }
}

impl<const SYSTEM: SystemId> System<SYSTEM> {
    pub fn insert_dvd(&mut self, dvd: Box<dyn image::Dvd>) {
        let name = String::from_utf8_lossy(&dvd.header().game_name);
        let name = name.trim_end_matches('\0');
        tracing::info!("DVD inserted: {}", name);
        self.di.dvd = Some(dvd);
        self.close_cover();
    }

    pub fn open_cover(&mut self) {
        tracing::debug!("DVD drive cover opened");
        self.di.cover = self.di.cover.with_cover_interrupt(true).with_cover_status(true);
        self::refresh_interrupts(self);
    }

    pub fn close_cover(&mut self) {
        tracing::debug!("DVD drive cover closed");
        self.di.cover = self.di.cover.with_cover_status(false).with_cover_interrupt(false);
        self::refresh_interrupts(self);
    }
}

impl DvdInterface {
    pub fn save_state(&self, w: &mut crate::savestate::StateWriter) {
        w.pod(&self.status);
        w.pod(&self.cover);
        w.pod(&self.dma_address);
        w.pod(&self.dma_length);
        w.pod(&self.control);
        w.pod(&self.config);
        w.u32(self.cmdbuf0);
        w.u32(self.cmdbuf1);
        w.u32(self.cmdbuf2);
        w.u32(self.immbuf);
    }

    pub fn load_state(
        &mut self,
        r: &mut crate::savestate::StateReader<'_>,
    ) -> Result<(), crate::savestate::StateError> {
        self.status = r.pod()?;
        self.cover = r.pod()?;
        self.dma_address = r.pod()?;
        self.dma_length = r.pod()?;
        self.control = r.pod()?;
        self.config = r.pod()?;
        self.cmdbuf0 = r.u32()?;
        self.cmdbuf1 = r.u32()?;
        self.cmdbuf2 = r.u32()?;
        self.immbuf = r.u32()?;
        Ok(())
    }
}
