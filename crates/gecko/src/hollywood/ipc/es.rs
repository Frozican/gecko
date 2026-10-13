use image::dvd::PARTITION_TMD_SIZE_OFFSET;

use crate::hollywood::ipc::IosDevice;

pub const IOCTL_GET_NUM_TICKET_VIEWS: u32 = 0x12;
pub const IOCTL_GET_CONSUMPTION: u32 = 0x16;
pub const IOCTL_DI_GET_TICKET_VIEW: u32 = 0x1B;
pub const IOCTL_GET_DATA_DIR: u32 = 0x1D;
pub const IOCTL_GET_TITLE_ID: u32 = 0x20;
pub const IOCTL_DI_GET_TMD_SIZE: u32 = 0x39;

pub const ES_EINVAL: i32 = -1017;
pub const ES_ENOMEM: i32 = -1024;
pub const ES_NO_TICKET: i32 = -1028;
pub const ES_INVALID_TICKET: i32 = -1029;

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
            IOCTL_GET_NUM_TICKET_VIEWS => {
                if in_count != 1 || io_count != 1 {
                    return ES_EINVAL;
                }
                if ctx.mmio.phys_read_u32(vec_ptr + 4) != 8 || ctx.mmio.phys_read_u32(vec_ptr + 12) != 4 {
                    return ES_EINVAL;
                }

                let in_buf = ctx.mmio.phys_read_u32(vec_ptr);
                let title_id =
                    ((ctx.mmio.phys_read_u32(in_buf) as u64) << 32) | ctx.mmio.phys_read_u32(in_buf + 4) as u64;
                let out_buf = ctx.mmio.phys_read_u32(vec_ptr + 8);

                let ios_number = title_id as u32;
                let available =
                    title_id >> 32 == 1 && ios_number < 0x100 && crate::wii::ios::for_ios(ios_number as u16).is_some();
                let count = available as u32;
                ctx.mmio.phys_write_u32(out_buf, count);

                tracing::debug!(title_id = format!("{title_id:016X}"), count, "ES_GetNumTicketViews");
                0
            }
            IOCTL_GET_TITLE_ID => {
                if in_count != 0 || io_count != 1 {
                    tracing::warn!(in_count, io_count, "ES_GetTitleID: unexpected vector counts");
                    return ES_EINVAL;
                }

                let out_buf = ctx.mmio.phys_read_u32(vec_ptr);
                let Some(dvd) = ctx.di.dvd.as_ref() else {
                    return ES_EINVAL;
                };

                ctx.mmio.phys_write_u32(out_buf, (dvd.tmd_title_id() >> 32) as u32);
                ctx.mmio.phys_write_u32(out_buf + 4, dvd.tmd_title_id() as u32);

                tracing::debug!(
                    title_id = format!("{:016X}", dvd.tmd_title_id()),
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

                let in_buf = ctx.mmio.phys_read_u32(vec_ptr);
                let requested_title_id =
                    ((ctx.mmio.phys_read_u32(in_buf) as u64) << 32) | (ctx.mmio.phys_read_u32(in_buf + 4) as u64);

                let path = format!(
                    "/title/{:08x}/{:08x}/data",
                    (requested_title_id >> 32) as u32,
                    requested_title_id as u32
                );

                let out_buf = ctx.mmio.phys_read_u32(vec_ptr + 8);
                let out_len = ctx.mmio.phys_read_u32(vec_ptr + 12);
                if (path.len() as u32) + 1 > out_len {
                    tracing::warn!(
                        out_len,
                        needed = path.len() + 1,
                        "ES_GetDataDir: output buffer too small"
                    );
                    return ES_EINVAL;
                }

                for (i, b) in path.bytes().enumerate() {
                    ctx.mmio.phys_write_u8(out_buf + i as u32, b);
                }
                ctx.mmio.phys_write_u8(out_buf + path.len() as u32, 0);

                tracing::debug!(
                    title_id = format!("{requested_title_id:016X}"),
                    %path,
                    out_buf = format!("{out_buf:#010X}"),
                    "ES_GetDataDir"
                );
                0
            }
            IOCTL_DI_GET_TMD_SIZE => {
                if in_count != 0 || io_count != 1 {
                    return ES_EINVAL;
                }
                if ctx.mmio.phys_read_u32(vec_ptr + 4) != 4 {
                    return ES_EINVAL;
                }
                let Some(dvd) = ctx.di.dvd.as_ref() else {
                    return ES_EINVAL;
                };

                let tmd_size_addr = dvd.data_partition_offset() as usize + PARTITION_TMD_SIZE_OFFSET;

                let mut size = [0u8; 4];
                dvd.read_raw_disc(tmd_size_addr, &mut size);
                let size = u32::from_be_bytes(size);

                tracing::debug!(addr = format!("{:#010X}", tmd_size_addr), size, "ES_DIGetTmdSize");

                let out_buf = ctx.mmio.phys_read_u32(vec_ptr);
                ctx.mmio.phys_write_u32(out_buf, size);

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
