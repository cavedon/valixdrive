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

use anyhow::Result;
use std::time;

mod linux;

/// A trait for storage device operations.
pub trait Device {
    /// Returns the size of the device in bytes.
    fn get_size(&self) -> u64;
    /// Returns the device information.
    fn get_device_info(&mut self) -> Result<&DeviceInfo>;
    /// Reads data from the device at the given offset.
    /// Returns the time spent reading data.
    fn read(&mut self, offset: u64, data: &mut [u8]) -> Result<time::Duration>;
    /// Writes data to the device at the given offset.
    /// Returns the time spent writing data.
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<time::Duration>;
    /// Returns the block size (in bytes) memory operations needs to be aligned
    /// to for this device.
    fn get_memory_alignment(&self) -> usize;
}

/// Information about a storage device.
pub struct DeviceInfo {
    pub vendor: String,
    pub model: String,
    pub serial: String,
    pub revision: String,
    pub firmware_revision: String,
    pub size: u64,
    pub is_block_device: bool,
    pub logical_block_size: u64,
    pub physical_block_size: u64,
    pub subsystems: Vec<String>,
    pub usb_driver: String,
    pub usb_vendor_id: String,
    pub usb_product_id: String,
    pub usb_manufacturer: String,
    pub usb_product: String,
    pub usb_serial_number: String,
    pub usb_version: String,
    pub usb_speed: String,
}

impl DeviceInfo {
    pub fn new() -> DeviceInfo {
        DeviceInfo {
            vendor: String::new(),
            model: String::new(),
            serial: String::new(), // Add the missing field 'serial'
            revision: String::new(),
            firmware_revision: String::new(), // Add the missing field 'firmware_revision'
            size: 0,
            is_block_device: false,
            logical_block_size: 0,
            physical_block_size: 0,
            subsystems: Vec::new(),
            usb_vendor_id: String::new(),
            usb_product_id: String::new(),
            usb_manufacturer: String::new(),
            usb_product: String::new(),
            usb_serial_number: String::new(),
            usb_version: String::new(),
            usb_speed: String::new(),
            usb_driver: String::new(), // Add the missing field 'usb_driver'
        }
    }

    /// Prints the device information to stdout.
    pub fn print(&self) {
        print_if_not_empty("Vendor", &self.vendor);
        print_if_not_empty("Model", &self.model);
        print_if_not_empty("Serial number", &self.serial);
        print_if_not_empty("Revision", &self.revision);
        print_if_not_empty("Firmware revision", &self.firmware_revision);
        println!(
            "Device size: {} bytes ({:.3} GiB, {:.3} GB)",
            self.size,
            self.size as f64 / 1024.0 / 1024.0 / 1024.0,
            self.size as f64 / 1_000_000_000.0,
        );
        if self.is_block_device {
            println!(
                "Block size (physical/logical): {}/{} bytes",
                self.physical_block_size, self.logical_block_size
            );
        }
        print_if_not_empty("Subsystems", &self.subsystems.join(", "));
        print_if_not_empty("USB driver", &self.usb_driver);
        if !self.usb_vendor_id.is_empty() || !self.usb_product_id.is_empty() {
            println!(
                "USB vendor/product ID: {}:{}",
                self.usb_vendor_id, self.usb_product_id
            );
        }
        print_if_not_empty("USB manufacturer", &self.usb_manufacturer);
        print_if_not_empty("USB product", &self.usb_product);
        print_if_not_empty("USB serial number", &self.usb_serial_number);
        if !self.usb_version.is_empty() || !self.usb_speed.is_empty() {
            println!(
                "USB version (speed): {} ({} Mbps)",
                self.usb_version, self.usb_speed
            );
        }
    }
}

/// Opens the storage device at the given path.
///
/// If `read_only` is true, the device is opened in read-only mode.
pub fn open(device: &str, read_only: bool) -> Result<Box<dyn Device>> {
    Ok(Box::new(linux::open(device, read_only)?) as Box<dyn Device>)
}

/// If `value` is not empty, prints `label: value` to stdout.
fn print_if_not_empty(label: &str, value: &str) {
    if !value.is_empty() {
        println!("{}: {}", label, value);
    }
}
