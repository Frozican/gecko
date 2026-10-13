use crate::hollywood::ipc::IosDevice;
use crate::wii::ios::MemoryValues;

pub const IOCTL_LAUNCH: u32 = 0x08;
pub const IOCTL_GET_NUM_TICKET_VIEWS: u32 = 0x12;
pub const IOCTL_GET_TICKET_VIEWS: u32 = 0x13;
pub const IOCTL_GET_CONSUMPTION: u32 = 0x16;
pub const IOCTL_DI_GET_TICKET_VIEW: u32 = 0x1B;
pub const IOCTL_GET_DATA_DIR: u32 = 0x1D;
pub const IOCTL_GET_TITLE_ID: u32 = 0x20;
pub const IOCTL_DI_GET_TMD_SIZE: u32 = 0x39;
pub const IOCTL_DI_GET_TMD: u32 = 0x3A;

pub const ES_EINVAL: i32 = -1017;
pub const ES_ENOMEM: i32 = -1024;
pub const ES_NO_TICKET: i32 = -1028;
pub const ES_INVALID_TICKET: i32 = -1029;

const TICKET_VIEW_SIZE: u32 = 0xD8;

pub struct ETicketServices;

impl IosDevice for ETicketServices {
    fn ioctlv(
        &mut self,
        ctx: &mut super::DeviceContext<'_>,
        cmd: u32,
        in_count: u32,
        io_count: u32,
        vec_ptr: u32,
    ) -> i32 {
        match cmd {
            IOCTL_LAUNCH => {
                if in_count != 2 || io_count != 0 || !super::vec_lens_match(ctx.mmio, vec_ptr, &[8, TICKET_VIEW_SIZE]) {
                    return ES_EINVAL;
                }

                let title_id = self::read_title_id(ctx, vec_ptr);
                match TitleKind::of(title_id) {
                    TitleKind::Ios(ios) => {
                        let Some(imv) = crate::wii::ios::for_ios(ios) else {
                            tracing::warn!(ios, "ES_LaunchTitle: no HLE values for IOS");
                            return super::fs::FS_ENOENT;
                        };

                        tracing::debug!(title_id = format!("{title_id:016X}"), "ES_LaunchTitle: reload IOS");
                        ctx.reload_ios = Some(imv);
                        0
                    }
                    kind @ (TitleKind::GameCube | TitleKind::Broadway) => {
                        // These reset Broadway (lol thanks zayd) into another title, which needs installed titles we don't have.
                        tracing::error!(
                            title_id = format!("{title_id:016X}"),
                            ?kind,
                            "ES_LaunchTitle: unsupported title kind"
                        );
                        super::fs::FS_ENOENT
                    }
                }
            }
            IOCTL_GET_NUM_TICKET_VIEWS => {
                if in_count != 1 || io_count != 1 || !super::vec_lens_match(ctx.mmio, vec_ptr, &[8, 4]) {
                    return ES_EINVAL;
                }

                let title_id = self::read_title_id(ctx, vec_ptr);
                let count = self::hle_ios(title_id).is_some() as u32;
                ctx.mmio.phys_write_u32(super::vec_data(ctx.mmio, vec_ptr, 1), count);

                tracing::debug!(title_id = format!("{title_id:016X}"), count, "ES_GetNumTicketViews");
                0
            }
            IOCTL_GET_TICKET_VIEWS => {
                if in_count != 2 || io_count != 1 || !super::vec_lens_match(ctx.mmio, vec_ptr, &[8, 4]) {
                    return ES_EINVAL;
                }

                let title_id = self::read_title_id(ctx, vec_ptr);
                let requested_count = ctx.mmio.phys_read_u32(super::vec_data(ctx.mmio, vec_ptr, 1));
                let has_view = self::hle_ios(title_id).is_some() && requested_count != 0;
                if has_view {
                    if super::vec_len(ctx.mmio, vec_ptr, 2) < TICKET_VIEW_SIZE {
                        return ES_EINVAL;
                    }

                    let out_buf = super::vec_data(ctx.mmio, vec_ptr, 2);
                    let view = ctx.mmio.phys_slice_mut(out_buf, TICKET_VIEW_SIZE as usize);
                    view.fill(0);
                    view[0x10..0x18].copy_from_slice(&title_id.to_be_bytes());
                }

                tracing::debug!(
                    title_id = format!("{title_id:016X}"),
                    requested_count,
                    count = has_view as u32,
                    "ES_GetTicketViews"
                );
                0
            }
            IOCTL_GET_TITLE_ID => {
                if in_count != 0 || io_count != 1 {
                    tracing::warn!(in_count, io_count, "ES_GetTitleID: unexpected vector counts");
                    return ES_EINVAL;
                }

                let out_buf = super::vec_data(ctx.mmio, vec_ptr, 0);
                let Some(dvd) = ctx.di.dvd.as_ref() else {
                    return ES_EINVAL;
                };

                let title_id = dvd.tmd_title_id();
                ctx.mmio.phys_write_u64(out_buf, title_id);

                tracing::debug!(
                    title_id = format!("{title_id:016X}"),
                    out_buf = format!("{out_buf:#010X}"),
                    "ES_GetTitleID"
                );
                0
            }
            IOCTL_GET_CONSUMPTION => {
                tracing::warn!(in_count, io_count, "ES_GetConsumption: stubbed");
                0
            }
            IOCTL_DI_GET_TICKET_VIEW => {
                tracing::warn!(in_count, io_count, "ES_DIGetTicketView: stubbed");
                0
            }
            IOCTL_GET_DATA_DIR => {
                if in_count != 1 || io_count != 1 {
                    tracing::warn!(in_count, io_count, "ES_GetDataDir: unexpected vector counts");
                    return ES_EINVAL;
                }

                let requested_title_id = self::read_title_id(ctx, vec_ptr);
                let path = format!(
                    "/title/{:08x}/{:08x}/data",
                    (requested_title_id >> 32) as u32,
                    requested_title_id as u32
                );

                let out_buf = super::vec_data(ctx.mmio, vec_ptr, 1);
                let out_len = super::vec_len(ctx.mmio, vec_ptr, 1);
                if (path.len() as u32) + 1 > out_len {
                    tracing::warn!(
                        out_len,
                        needed = path.len() + 1,
                        "ES_GetDataDir: output buffer too small"
                    );
                    return ES_EINVAL;
                }

                let out = ctx.mmio.phys_slice_mut(out_buf, path.len() + 1);
                out[..path.len()].copy_from_slice(path.as_bytes());
                out[path.len()] = 0;

                tracing::debug!(
                    title_id = format!("{requested_title_id:016X}"),
                    %path,
                    out_buf = format!("{out_buf:#010X}"),
                    "ES_GetDataDir"
                );
                0
            }
            IOCTL_DI_GET_TMD_SIZE => {
                if in_count != 0 || io_count != 1 || !super::vec_lens_match(ctx.mmio, vec_ptr, &[4]) {
                    return ES_EINVAL;
                }
                let Some(dvd) = ctx.di.dvd.as_ref() else {
                    return ES_EINVAL;
                };

                let (size, _) = dvd.tmd_descriptor();
                ctx.mmio.phys_write_u32(super::vec_data(ctx.mmio, vec_ptr, 0), size);

                tracing::debug!(size, "ES_DIGetTmdSize");
                0
            }
            IOCTL_DI_GET_TMD => {
                if in_count != 1 || io_count != 1 || !super::vec_lens_match(ctx.mmio, vec_ptr, &[4]) {
                    return ES_EINVAL;
                }

                let requested_size = ctx.mmio.phys_read_u32(super::vec_data(ctx.mmio, vec_ptr, 0));
                let out_buf = super::vec_data(ctx.mmio, vec_ptr, 1);
                let out_len = super::vec_len(ctx.mmio, vec_ptr, 1);
                if requested_size != out_len {
                    return ES_EINVAL;
                }

                let Some(dvd) = ctx.di.dvd.as_ref() else {
                    return ES_EINVAL;
                };

                let (size, tmd_addr) = dvd.tmd_descriptor();
                if out_len < size {
                    return ES_EINVAL;
                }

                dvd.read_raw_disc(tmd_addr, ctx.mmio.phys_slice_mut(out_buf, size as usize));

                tracing::debug!(
                    addr = format!("{tmd_addr:#010X}"),
                    size,
                    out_buf = format!("{out_buf:#010X}"),
                    "ES_DIGetTmd"
                );
                0
            }
            _ => {
                tracing::warn!(
                    device = &ctx.device_path,
                    cmd = format!("{cmd:#010X}"),
                    in_count,
                    io_count,
                    vec_ptr = format!("{vec_ptr:#010X}"),
                    "ETicketServices: unimplemented ioctlv"
                );
                ES_EINVAL
            }
        }
    }
}

fn read_title_id(ctx: &super::DeviceContext<'_>, vec_ptr: u32) -> u64 {
    ctx.mmio.phys_read_u64(super::vec_data(ctx.mmio, vec_ptr, 0))
}

fn hle_ios(title_id: u64) -> Option<&'static MemoryValues> {
    match TitleKind::of(title_id) {
        TitleKind::Ios(ios) => crate::wii::ios::for_ios(ios),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleKind {
    Ios(u16),
    GameCube,
    Broadway,
}

impl TitleKind {
    const SYSTEM: u32 = 0x0000_0001;
    const SYSTEM_MENU: u32 = 0x0000_0002;
    const BC: u32 = 0x0000_0100;
    const MIOS: u32 = 0x0000_0101;

    fn of(title_id: u64) -> Self {
        match ((title_id >> 32) as u32, title_id as u32) {
            (Self::SYSTEM, Self::SYSTEM_MENU) => Self::Broadway,
            (Self::SYSTEM, Self::BC | Self::MIOS) => Self::GameCube,
            (Self::SYSTEM, ios @ 0x03..=0xFF) => Self::Ios(ios as u16),
            _ => Self::Broadway,
        }
    }
}
