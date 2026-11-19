//! Windows implementation for accessing a raw physical drive.
//!
//! Use a path such as `\\.\PhysicalDrive0`. Opening a physical drive generally
//! requires Administrator privileges. This backend uses buffered I/O.

use super::{Device, DeviceInfo};
use anyhow::{bail, Context, Result};
use std::{
    ffi::OsStr,
    io,
    os::windows::ffi::OsStrExt,
    ptr::{null, null_mut},
    time::{self, Instant},
};

type Handle = *mut core::ffi::c_void;
type Bool = i32;
type Dword = u32;

const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

const GENERIC_READ: Dword = 0x8000_0000;
const GENERIC_WRITE: Dword = 0x4000_0000;
const FILE_SHARE_READ: Dword = 0x0000_0001;
const FILE_SHARE_WRITE: Dword = 0x0000_0002;
const OPEN_EXISTING: Dword = 3;
const FILE_ATTRIBUTE_NORMAL: Dword = 0x0000_0080;
const FILE_BEGIN: Dword = 0;

const FSCTL_LOCK_VOLUME: Dword = 0x0009_0018;
const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: Dword = 0x0007_00A0;

#[repr(C)]
#[derive(Clone, Copy)]
struct DiskGeometry {
    cylinders: i64,
    media_type: Dword,
    tracks_per_cylinder: Dword,
    sectors_per_track: Dword,
    bytes_per_sector: Dword,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DiskGeometryEx {
    geometry: DiskGeometry,
    disk_size: i64,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        filename: *const u16,
        desired_access: Dword,
        share_mode: Dword,
        security_attributes: *mut core::ffi::c_void,
        creation_disposition: Dword,
        flags_and_attributes: Dword,
        template_file: Handle,
    ) -> Handle;

    fn CloseHandle(handle: Handle) -> Bool;

    fn DeviceIoControl(
        device: Handle,
        control_code: Dword,
        in_buffer: *const core::ffi::c_void,
        in_buffer_size: Dword,
        out_buffer: *mut core::ffi::c_void,
        out_buffer_size: Dword,
        bytes_returned: *mut Dword,
        overlapped: *mut core::ffi::c_void,
    ) -> Bool;

    fn SetFilePointerEx(
        file: Handle,
        distance_to_move: i64,
        new_file_pointer: *mut i64,
        move_method: Dword,
    ) -> Bool;

    fn ReadFile(
        file: Handle,
        buffer: *mut u8,
        bytes_to_read: Dword,
        bytes_read: *mut Dword,
        overlapped: *mut core::ffi::c_void,
    ) -> Bool;

    fn WriteFile(
        file: Handle,
        buffer: *const u8,
        bytes_to_write: Dword,
        bytes_written: *mut Dword,
        overlapped: *mut core::ffi::c_void,
    ) -> Bool;
}

pub struct WindowsDevice {
    handle: Handle,
    info: DeviceInfo,
    sector_size: u32,
    read_only: bool,
}

impl Drop for WindowsDevice {
    fn drop(&mut self) {
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

/// Open a raw physical drive, query its geometry, and only then return it.
///
/// `device` should be a path such as `\\.\PhysicalDrive0`.
pub fn open(device: &str, read_only: bool) -> Result<WindowsDevice> {
    let path: Vec<u16> = OsStr::new(device).encode_wide().chain(Some(0)).collect();

    let access = if read_only {
        GENERIC_READ
    } else {
        GENERIC_READ | GENERIC_WRITE
    };

    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(last_os_error_with_context_closure(|| {
            format!("opening Windows drive {device:?}")
        }));
    }

    let mut bytes_returned: u32 = 0;

    let locked = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_LOCK_VOLUME,
            null(),
            0,
            null_mut(),
            0,
            &mut bytes_returned,
            null_mut(),
        )
    };

    if locked == 0 {
        return Err(last_os_error_with_context_closure(|| {
            format!("locking Windows drive {device:?}")
        }));
    }

    // Query geometry before returning the device. If the query fails or reports
    // invalid geometry, close the handle and do not enable writes.
    let geometry = match query_geometry(handle) {
        Ok(geometry) => geometry,
        Err(err) => {
            unsafe {
                CloseHandle(handle);
            }
            return Err(err).with_context(|| {
                format!("querying geometry for Windows drive {device:?}")
            });
        }
    };

    let mut info = DeviceInfo::new();
    info.size = geometry.disk_size as u64;
    info.is_block_device = true;
    info.logical_block_size = u64::from(geometry.geometry.bytes_per_sector);
    // This ioctl reports logical sector size. It does not identify the
    // physical sector size, so do not claim that it does.
    info.physical_block_size = 0;

    Ok(WindowsDevice {
        handle,
        info,
        sector_size: geometry.geometry.bytes_per_sector,
        read_only,
    })
}

fn query_geometry(handle: Handle) -> Result<DiskGeometryEx> {
    // DISK_GEOMETRY_EX is followed by a variable-length data area. The fixed
    // fields we need fit in this buffer.
    let mut buffer = [0u8; 256];
    let mut bytes_returned = 0;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            null(),
            0,
            buffer.as_mut_ptr().cast(),
            buffer.len() as Dword,
            &mut bytes_returned,
            null_mut(),
        )
    };

    if ok == 0 {
        return Err(last_os_error_with_context_string(
            "IOCTL_DISK_GET_DRIVE_GEOMETRY_EX failed"
        ));
    }

    let required = core::mem::size_of::<DiskGeometryEx>();
    if (bytes_returned as usize) < required {
        bail!(
            "drive geometry response was too short: received {bytes_returned} bytes, need at least {required}"
        );
    }

    // The ioctl output buffer is byte-aligned; read_unaligned avoids relying
    // on the stack buffer's alignment for this C-layout structure.
    let geometry = unsafe {
        core::ptr::read_unaligned(buffer.as_ptr().cast::<DiskGeometryEx>())
    };

    if geometry.disk_size <= 0 {
        bail!("drive reported an invalid size: {}", geometry.disk_size);
    }
    if geometry.geometry.bytes_per_sector == 0 {
        bail!("drive reported a zero-byte logical sector size");
    }

    Ok(geometry)
}

impl Device for WindowsDevice {
    fn get_size(&self) -> u64 {
        self.info.size
    }

    fn get_device_info(&mut self) -> Result<&DeviceInfo> {
        Ok(&self.info)
    }

    fn read(&mut self, offset: u64, data: &mut [u8]) -> Result<time::Duration> {
        self.check_io_range(offset, data.len())?;
        seek_to(self.handle, offset)?;

        let start = Instant::now();
        let mut done = 0;

        while done < data.len() {
            let remaining = data.len() - done;
            let chunk_len = remaining.min(Dword::MAX as usize) as Dword;
            let mut bytes_read = 0;

            let ok = unsafe {
                ReadFile(
                    self.handle,
                    data[done..].as_mut_ptr(),
                    chunk_len,
                    &mut bytes_read,
                    null_mut(),
                )
            };

            if ok == 0 {
                return Err(last_os_error_with_context_closure(|| {
                    format!("reading at device offset {}", offset + done as u64)
                }));
            }
            if bytes_read == 0 {
                bail!("unexpected end of device while reading at offset {}", offset + done as u64);
            }

            done += bytes_read as usize;
        }

        Ok(start.elapsed())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<time::Duration> {
        if self.read_only {
            bail!("device was opened read-only");
        }
        self.check_io_range(offset, data.len())?;
        seek_to(self.handle, offset)?;

        let start = Instant::now();
        let mut done = 0;

        while done < data.len() {
            let remaining = data.len() - done;
            let chunk_len = remaining.min(Dword::MAX as usize) as Dword;
            let mut bytes_written = 0;

            let ok = unsafe {
                WriteFile(
                    self.handle,
                    data[done..].as_ptr(),
                    chunk_len,
                    &mut bytes_written,
                    null_mut(),
                )
            };

            if ok == 0 {
                return Err(last_os_error_with_context_closure(|| {
                    format!("writing at device offset {}", offset + done as u64)
                }));
            }
            if bytes_written == 0 {
                bail!("zero bytes written at device offset {}", offset + done as u64);
            }

            done += bytes_written as usize;
        }

        Ok(start.elapsed())
    }

    // Buffered Windows I/O does not require the user's buffer address to be
    // sector-aligned. Offset and length alignment are enforced separately.
    fn get_memory_alignment(&self) -> usize {
        1
    }
}

impl WindowsDevice {
    fn check_io_range(&self, offset: u64, len: usize) -> Result<()> {
        let sector = u64::from(self.sector_size);

        if offset % sector != 0 {
            bail!("device offset {offset} is not aligned to the {sector}-byte logical sector size");
        }
        if (len as u64) % sector != 0 {
            bail!("I/O length {len} is not a multiple of the {sector}-byte logical sector size");
        }

        let end = offset
            .checked_add(len as u64)
            .context("device I/O range overflow")?;
        if end > self.info.size {
            bail!(
                "I/O range {offset}..{end} exceeds device size {}",
                self.info.size
            );
        }

        Ok(())
    }
}

fn seek_to(handle: Handle, offset: u64) -> Result<()> {
    let offset = i64::try_from(offset).context("device offset exceeds Windows file-position range")?;
    let ok = unsafe { SetFilePointerEx(handle, offset, null_mut(), FILE_BEGIN) };

    if ok == 0 {
        return Err(last_os_error_with_context_string(
            "seeking to device offset"
        ));
    }

    Ok(())
}

fn last_os_error_with_context_closure<F, C>(context: F) -> anyhow::Error
where
    F: FnOnce() -> C,
    C: std::fmt::Display + Send + Sync + 'static,
{
    let err = io::Error::last_os_error();
    eprintln!(
        "OS error: {err:?}, code: {:?}",
        err.raw_os_error()
    );

    Err::<(), _>(err)
        .with_context(context)
        .unwrap_err()
}

fn last_os_error_with_context_string(context: impl Into<String>) -> anyhow::Error {
    let err = std::io::Error::last_os_error();
    eprintln!(
        "OS error: {err:?}, code: {:?}",
        err.raw_os_error()
    );

    Err::<(), _>(err)
        .context(context.into())
        .unwrap_err()
}
