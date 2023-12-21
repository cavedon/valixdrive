/*
Copyright (c) 2024 Ludovico Cavedon <ludovico.cavedon@gmail.com>

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

///! Linux implementation for accessing a storage device.
use anyhow::{Context, Result};
use std::{
    cmp::max,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path, time,
};

use super::DeviceInfo;

/// Struct implementing the Device trait for Linux.
pub struct LinuxDevice {
    path: String,
    drive: File,
    size: u64,
    device_info: DeviceInfo,
    has_device_info: bool,
    memory_alignment: usize,
}

pub fn open(device: &str, read_only: bool) -> Result<LinuxDevice> {
    let mut options = OpenOptions::new();
    options.read(true);
    let mut flags = libc::O_DIRECT | libc::O_SYNC;
    if !read_only {
        options.write(true);
        flags |= libc::O_EXCL;
    }
    options.custom_flags(flags);
    let mut drive = options
        .open(device)
        .context(format!("opening {}", device))?;
    let size = drive
        .seek(SeekFrom::End(0))
        .context(format!("seeking to end of device {}", device))?;
    let mut device_info = DeviceInfo::new();
    device_info.size = size;
    Ok(LinuxDevice {
        path: String::from(device),
        drive,
        size,
        device_info,
        has_device_info: false,
        memory_alignment: 0,
    })
}

impl super::Device for LinuxDevice {
    fn get_size(&self) -> u64 {
        self.size
    }

    fn get_device_info(&mut self) -> Result<&DeviceInfo> {
        if !self.has_device_info {
            self.fill_device_info()?;
            self.has_device_info = true
        }
        Ok(&self.device_info)
    }

    fn read(&mut self, offset: u64, data: &mut [u8]) -> Result<time::Duration> {
        self.drive.seek(SeekFrom::Start(offset)).context(format!(
            "seeking to offset {offset} in drive {:?}",
            self.drive
        ))?;
        let start = time::Instant::now();
        self.drive.read_exact(data).context(format!(
            "reading at offset {offset} from drive {:?}",
            self.drive
        ))?;
        let duration = start.elapsed();
        Ok(duration)
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<time::Duration> {
        self.drive.seek(SeekFrom::Start(offset)).context(format!(
            "seeking at offset {offset} in drive {:?}",
            self.drive
        ))?;
        let start = time::Instant::now();
        self.drive.write_all(data).context(format!(
            "writing at offset {offset} on drive {:?}",
            self.drive
        ))?;
        let duration = start.elapsed();
        Ok(duration)
    }

    fn get_memory_alignment(&self) -> usize {
        self.memory_alignment
    }
}

impl LinuxDevice {
    /// Populate the device information struct reading data from block device
    /// ioctls and sysfs.
    fn fill_device_info(&mut self) -> Result<()> {
        let block_dev = match io_block::os::BlockDev::from_file(
            self.drive
                .try_clone()
                .context(format!("reading block device properties of {}", self.path))?,
        ) {
            Ok(block_dev) => block_dev,
            Err(err) => {
                if err.kind() == ErrorKind::InvalidInput {
                    println!("Warning: {} is not a block device", self.path);
                    return Ok(());
                } else {
                    return Err(err)
                        .context(format!("reading block device properties of {}", self.path));
                }
            }
        };
        self.device_info.is_block_device = true;
        self.device_info.logical_block_size =
            io_block::BlockSize::block_size_logical(&block_dev)
                .context(format!("reading logical block size of {}", self.path))?;
        self.device_info.physical_block_size = io_block::BlockSize::block_size_physical(&block_dev)
            .context(format!("reading physical block size of {}", self.path))?;
        // When opening a block device with O_DIRECT, I/O operations needs to be aligned to the
        // block size. Taking the maximum of the logical and physical block size should be safe.
        self.memory_alignment = max(
            self.device_info.logical_block_size,
            self.device_info.physical_block_size,
        ) as usize;
        // Despite the name, block_count returns the size in bytes: https://github.com/jmesmon/io-block/issues/4
        let size = io_block::BlockSize::block_count(&block_dev)
            .context(format!("reading device size of {}", self.path))?;
        if size != self.device_info.size {
            // Safeguard check. It should never happen.
            return Err(anyhow::anyhow!(
                "Block device size ({} bytes) does not match the size reported by seeking to the end of the device ({} bytes)",
                size,
                self.device_info.size
            ));
        }
        let devno = parse_devno(
            self.drive
                .metadata()
                .context(format!("reading device metadata of {}", self.path))?
                .rdev(),
        );
        let sys_path = get_sys_path_for_devno(&devno);
        self.device_info.vendor = read_and_trim(sys_path.join("device/vendor").as_path());
        self.device_info.model = read_and_trim(sys_path.join("device/model").as_path());
        self.device_info.serial = read_and_trim(sys_path.join("device/serial").as_path());
        self.device_info.revision = read_and_trim(sys_path.join("device/rev").as_path());
        self.device_info.firmware_revision =
            read_and_trim(sys_path.join("device/firmware_rev").as_path());
        self.device_info.subsystems = get_subsystems_for_sys_path(&sys_path)
            .context(format!("getting subsystems for sys path {:?}", sys_path))?;
        if self.device_info.subsystems.contains(&String::from("usb")) {
            self.fill_usb_device_info(&sys_path)?;
        }
        Ok(())
    }

    /// Populate the USB device information struct reading data from sysfs.
    fn fill_usb_device_info(&mut self, sys_path: &path::Path) -> Result<()> {
        // We traverse the sysfs tree upwards until we find a directory named "driver" in the "usb"
        // subsystem. The parent directory of "driver" contains the USB device information.
        // We stop traversing the tree if we find a directory named "sys", which is the root of the
        // sysfs tree.
        let sys_path_link =
            fs::canonicalize(&sys_path).context(format!("canonicalizing {:?}", sys_path))?;
        let mut path_iter = sys_path_link.as_path();
        while path_iter
            .file_name()
            .context(format!("getting base name from {:?}", path_iter))?
            != "sys"
        {
            let subsystem_path = path_iter.join("subsystem");
            if subsystem_path.exists() {
                let subsystem_link = subsystem_path
                    .read_link()
                    .context(format!("reading symlink {:?}", subsystem_path))?;
                if subsystem_link
                    .file_name()
                    .context(format!("getting base name from {:?}", subsystem_link))?
                    == "usb"
                {
                    let driver_path = path_iter.join("driver");
                    if driver_path.exists() {
                        let driver_link = driver_path
                            .read_link()
                            .context(format!("reading symlink {:?}", driver_path))?;
                        let driver = driver_link
                            .file_name()
                            .context(format!("getting base name from {:?}", driver_link))?;
                        // The USB driver is either "uas" (newer) or "usb-storage" (older).
                        if driver == "uas" || driver == "usb-storage" {
                            self.device_info.usb_driver = driver.to_string_lossy().to_string();
                            let parent = path_iter
                                .parent()
                                .context(format!("getting parent of {:?}", path_iter))?;
                            if parent.join("idVendor").exists() {
                                self.device_info.usb_vendor_id =
                                    read_and_trim(parent.join("idVendor").as_path());
                                self.device_info.usb_product_id =
                                    read_and_trim(parent.join("idProduct").as_path());
                                // Manufacturer and product reported by the USB subsystem often
                                // match those from the block device, but not always.
                                self.device_info.usb_manufacturer =
                                    read_and_trim(parent.join("manufacturer").as_path());
                                self.device_info.usb_product =
                                    read_and_trim(parent.join("product").as_path());
                                self.device_info.usb_serial_number =
                                    read_and_trim(parent.join("serial").as_path());
                                self.device_info.usb_version =
                                    read_and_trim(parent.join("version").as_path());
                                self.device_info.usb_speed =
                                    read_and_trim(parent.join("speed").as_path());
                                break;
                            }
                        }
                    }
                }
            }
            let parent_path = path_iter.parent();
            if parent_path.is_none() {
                break;
            }
            path_iter = parent_path.unwrap();
        }
        Ok(())
    }
}

struct DevNo {
    major: u32,
    minor: u32,
}

/// Parse a device number into a major and minor number.
fn parse_devno(devno: u64) -> DevNo {
    // From https://elixir.bootlin.com/linux/v5.19/source/include/linux/kdev_t.h#L46
    let major = (devno >> 8) & 0xfff;
    let minor = (devno & 0xff) | ((devno >> 12) & 0xfff00);
    DevNo {
        major: major as u32,
        minor: minor as u32,
    }
}

/// Get the sysfs path for a device number.
fn get_sys_path_for_devno(devno: &DevNo) -> path::PathBuf {
    let mut path = path::PathBuf::from("/sys/dev/block");
    path.push(format!("{}:{}", devno.major, devno.minor));
    path
}

/// Read a file into a string and trim whitespace.
/// Returns an empty string if the file does not exist.
fn read_and_trim(path: &path::Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(string) => string.trim().to_string(),
        Err(_) => String::new(),
    }
}

/// Get the list of subsystems for a sysfs path.
fn get_subsystems_for_sys_path(sys_path: &path::Path) -> Result<Vec<String>> {
    let mut subsystems = Vec::new();
    let sys_path_link =
        fs::canonicalize(sys_path).context(format!("canonicalizing {:?}", sys_path))?;
    let mut path_iter = sys_path_link.as_path();
    while path_iter
        .file_name()
        .context(format!("getting base name from {:?}", path_iter))?
        != "sys"
    {
        let subsystem_path = path_iter.join("subsystem");
        if subsystem_path.exists() {
            let subsystem_link = subsystem_path
                .read_link()
                .context(format!("reading symlink {:?}", subsystem_path))?;
            let subsystem_string = subsystem_link
                .file_name()
                .context(format!("getting base name from {:?}", subsystem_link))?
                .to_string_lossy()
                .to_string();
            if subsystems.last() != Some(&subsystem_string) {
                subsystems.push(subsystem_string);
            }
        }
        let parent_path = path_iter.parent();
        if parent_path.is_none() {
            break;
        }
        path_iter = parent_path.unwrap();
    }
    Ok(subsystems)
}
