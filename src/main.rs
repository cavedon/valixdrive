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

use anyhow::{anyhow, Result};
use clap::Parser;
use rand::{self, rngs, seq::SliceRandom, RngCore, SeedableRng};
use std::{
    ops::{DerefMut, Range},
    time::Duration,
};

mod device;

#[derive(Parser)]
#[clap(version = "1.0")]
struct Cli {
    /// The storage device to test.
    #[arg(short, long)]
    drive: String,
    /// The block size to read/write in KiB.
    #[arg(short = 'b', long = "block-size-kb", default_value = "4")]
    block_size_kb: u64,
    /// The number of blocks to test.
    #[arg(short = 'n', long = "num-blocks", default_value = "576")]
    num_blocks: usize,
    /// Perform only a read test.
    #[arg(short = 'R', long = "read-only")]
    read_only: bool,
    /// Width in columns of the validation map printed on the terminal.
    #[arg(short = 'w', long = "map-width", default_value = "64")]
    map_width: usize,
    /// Do not read and restore original blocks content.
    #[arg(short = 'O', long = "no-restore-original")]
    no_restore_original: bool,
}

/// Convert a Duration to milliseconds.
fn as_millis_f64(d: &Duration) -> f64 {
    d.as_nanos() as f64 / 1_000_000.0
}

/// Read all blocks identified by `spot_blocks`` from `drive`.
/// Read timings statistics are printed to stdout.
/// Returns a vector of blocks containing the read data and any errors.
fn read_blocks(
    drive: &mut dyn device::Device,
    spot_blocks: &Vec<BlockIdx>,
    block_size: usize,
) -> Blocks {
    let mut blocks = Blocks::new(block_size, spot_blocks.len(), drive.get_memory_alignment());

    let bar = indicatif::ProgressBar::new(spot_blocks.len() as u64);
    bar.set_style(
        indicatif::ProgressStyle::with_template("[ETA:{eta}] {bar:40.blue} {pos:>4}/{len:4} {msg}")
            .unwrap(),
    );
    bar.tick();
    let mut durations = Vec::with_capacity(spot_blocks.len());
    for i in 0..blocks.num_blocks {
        let offset = spot_blocks[i].num * block_size as u64;
        let data = &mut blocks.block_mut(i);
        match drive.read(offset, data) {
            Ok(duration) => {
                durations.push(duration);
            }
            Err(err) => {
                bar.suspend(|| {
                    println!(
                        "{}",
                        console::style(format!(
                            "Read error at block {} (offset {}): {}",
                            spot_blocks[i].idx, offset, err
                        ))
                        .red()
                    )
                });
                blocks.errors[i] = IoError::ReadError;
            }
        }
        bar.inc(1);
    }
    bar.finish();

    print_stats(&durations);
    blocks
}

/// Write the blocks identified by `spot_blocks` to `drive` with the data provided in `data`.
/// Blocks that are marked with a read error in `data` are skipped.
/// `data` is updated with any write errors.
/// Read timings statistics are printed to stdout.
fn write_blocks(drive: &mut dyn device::Device, spot_blocks: &Vec<BlockIdx>, data: &mut Blocks) {
    let bar = indicatif::ProgressBar::new(spot_blocks.len() as u64);
    bar.set_style(
        indicatif::ProgressStyle::with_template(
            "[ETA:{eta}] {bar:40.yellow} {pos:>4}/{len:4} {msg}",
        )
        .unwrap(),
    );
    bar.tick();
    let mut durations = Vec::with_capacity(spot_blocks.len());
    for i in 0..data.num_blocks {
        if data.errors[i] == IoError::ReadError {
            bar.inc(1);
            continue;
        }
        let offset = spot_blocks[i].num * data.block_size as u64;
        match drive.write(offset, data.block(i)) {
            Ok(duration) => {
                durations.push(duration);
            }
            Err(err) => {
                bar.suspend(|| {
                    println!(
                        "{}",
                        console::style(format!(
                            "Write error at block {} (offset {}): {}",
                            spot_blocks[i].idx, offset, err
                        ))
                        .red()
                    )
                });
                data.errors[i] = IoError::WriteError;
            }
        }
        bar.inc(1);
    }
    bar.finish();

    print_stats(&durations);
}

#[derive(Clone, PartialEq)]
enum IoError {
    None,
    ReadError,
    WriteError,
}

/// Structure holding the buffer for the blocks content.
struct Blocks {
    /// The buffer holding the blocks content. The blocks data starts at `start_offset` and the
    /// blocks are stored in the order they are read/written (not in the order they are present on
    /// the drive).
    data: Vec<u8>,
    /// The errors encountered when reading/writing the blocks. The vector has one element per
    /// block.
    errors: Vec<IoError>,
    /// The size of a block in bytes.
    block_size: usize,
    /// The offset in `data` where the blocks data starts. This is used to align the buffer to
    /// multiples of the sector size, required for O_DIRECT operations.
    start_offset: usize,
    /// The number of blocks to test.
    num_blocks: usize,
}

impl Blocks {
    /// Create a new `Blocks` structure with `num_blocks` blocks of size `block_size` bytes.
    /// The buffer is aligned to multiple of `mem_align` bytes.
    fn new(block_size: usize, num_blocks: usize, mem_align: usize) -> Self {
        // Align the beginning of the data stored in the buffer to multiples of `mem_align` bytes,
        // as it is required for O_DIRECT operations.
        // Using Rust's allocator_api would be a better solutions, but that feature is still
        // available only on nightly builds.
        let data = vec![0; num_blocks * block_size as usize + mem_align];
        let mut start_offset = 0;
        if mem_align > 0 && data.as_ptr() as usize % mem_align != 0 {
            start_offset = mem_align - data.as_ptr() as usize % mem_align;
        }
        Self {
            data,
            errors: vec![IoError::None; num_blocks],
            block_size,
            start_offset,
            num_blocks,
        }
    }

    /// Return the offset in `data` where the block with index `i` starts.
    fn block_offset(&self, i: usize) -> usize {
        self.start_offset + i * self.block_size
    }

    /// Return the range in `data` where the block with index `i` is stored.
    fn block_range(&self, i: usize) -> Range<usize> {
        self.block_offset(i)..self.block_offset(i + 1)
    }

    /// Return a reference to the block with index `i`.
    fn block(&self, i: usize) -> &[u8] {
        &self.data[self.block_range(i)]
    }

    /// Return a mutable reference to the block with index `i`.
    fn block_mut(&mut self, i: usize) -> &mut [u8] {
        let block_range = self.block_range(i);
        &mut self.data[block_range]
    }

    /// Return a mutable reference to the buffer holding the blocks data.
    fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

/// Structure holding the index of a block being tested and the corresponding
/// block number on the drive.
struct BlockIdx {
    idx: usize,
    num: u64,
}

/// Enumeration of the possible validation results for a block.
#[derive(Clone, PartialEq)]
enum BlockReport {
    Unknown,
    Validated,
    ReadError,
    ReadSuccessful,
    WriteError,
    NoStorage,
}

/// Print the validation map to stdout, with header and legend.
fn print_validation_map(validation_map: &Vec<BlockReport>, map_width: usize) {
    println!("{}", console::style("\nValidation map:").bold());
    for i in 0..validation_map.len() {
        match validation_map[i] {
            BlockReport::Validated => print!("{}", console::style("◼").green()),
            BlockReport::ReadError => print!("{}", console::style("R").blue()),
            BlockReport::ReadSuccessful => print!("{}", console::style("R").green()),
            BlockReport::WriteError => print!("{}", console::style("W").yellow()),
            BlockReport::NoStorage => print!("{}", console::style("✖").red()),
            // We should never have an un unknown block in the validation map.
            _ => print!("{}", console::style("?").white()),
        }
        if i % map_width == map_width - 1 {
            println!();
        }
    }
    if validation_map.len() % map_width != 0 {
        println!();
    }
    println!(
        "Legend: {} Validated   {} Read Error       {} Write Error",
        console::style("◼").green(),
        console::style("R").blue(),
        console::style("W").yellow(),
    );
    println!(
        "        {} No storage  {} Read Successful",
        console::style("✖").red(),
        console::style("R").green(),
    );
}

/// Print statistics about the duration of I/O operations.
fn print_stats(durations: &Vec<std::time::Duration>) {
    if durations.is_empty() {
        return;
    }
    let sum = durations.iter().sum::<std::time::Duration>();
    let avg = sum / durations.len() as u32;
    let variance = durations
        .iter()
        .map(|&duration| {
            let diff = as_millis_f64(&duration) - as_millis_f64(&avg);
            diff * diff
        })
        .sum::<f64>()
        / durations.len() as f64;
    let std_dev = variance.sqrt();
    // CV is the Coefficient of Variation.
    println!(
        "avg: {:.3} ms, stddev: {:.3} ms, CV: {:.3}",
        as_millis_f64(&avg),
        std_dev,
        std_dev / as_millis_f64(&avg)
    );

    // print min and max duration from durations
    let min = durations.iter().min().unwrap();
    let max = durations.iter().max().unwrap();
    println!(
        "min: {:.3} ms, max: {:.3} ms",
        as_millis_f64(min),
        as_millis_f64(max)
    );
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut drive = device::open(&cli.drive, cli.read_only)?;
    drive.get_device_info()?.print();

    if drive.get_size() % (cli.block_size_kb * 1024) != 0 {
        return Err(anyhow!(
            "The drive size ({} bytes) is not a multiple of the block size ({} KiB)",
            drive.get_size(),
            cli.block_size_kb
        ));
    }
    let num_drive_blocks = drive.get_size() / (cli.block_size_kb * 1024);
    // spot_blocks contains the list of blocks selected for testing.
    let mut spot_blocks = Vec::with_capacity(cli.num_blocks);
    for i in 0..cli.num_blocks {
        // Divide the drive in cli.num_blocks areas, and select the block best covering the end of
        // each area.
        spot_blocks.push(BlockIdx {
            idx: i,
            num: (((i + 1) as u64 * num_drive_blocks) as f64 / cli.num_blocks as f64).round()
                as u64
                - 1,
        });
    }

    let mut rng = rngs::SmallRng::from_entropy();
    // Shuffle the blocks to test, so that they are not tested in the order they are present on the
    // drive.
    spot_blocks.shuffle(&mut rng);

    // validation_map contains the result of the validation of each block.
    let mut validation_map = vec![BlockReport::Unknown; cli.num_blocks];

    // orig_data_option contains the original blocks data, if they were read, so that it can be
    // restored at the end of the test.
    let mut orig_data_option = None;

    if !cli.no_restore_original {
        println!("{}", console::style("\nReading original blocks").bold());
        let orig_data = read_blocks(
            drive.deref_mut(),
            &spot_blocks,
            cli.block_size_kb as usize * 1024,
        );

        // Record any read error in the validation map.
        for i in 0..cli.num_blocks {
            if orig_data.errors[i] == IoError::ReadError {
                validation_map[spot_blocks[i].idx] = BlockReport::ReadError;
            } else {
                validation_map[spot_blocks[i].idx] = BlockReport::ReadSuccessful;
            }
        }

        let has_read_errors = validation_map.contains(&BlockReport::ReadError);
        if has_read_errors || cli.read_only {
            // Typically, we would print the validation map at the end, but
            // if there were read errors, print the validation map and exit.
            print_validation_map(&validation_map, cli.map_width);
        }
        if cli.read_only {
            return Ok(());
        }
        if has_read_errors {
            println!(
                "{}",
                console::style("I/O errors encountered reading original blocks, exiting").red()
            );
            return Err(anyhow!("I/O errors reading original blocks"));
        }
        orig_data_option = Some(orig_data);
    }

    println!(
        "{}",
        console::style("\nWriting blocks with random data").bold()
    );

    // Generate the random data to write to the blocks.
    let mut random_blocks = Blocks::new(
        cli.block_size_kb as usize * 1024,
        cli.num_blocks,
        drive.get_memory_alignment(),
    );
    rng.fill_bytes(random_blocks.data_mut());

    write_blocks(drive.deref_mut(), &spot_blocks, &mut random_blocks);

    // Record any write error in the validation map.
    for i in 0..cli.num_blocks {
        if random_blocks.errors[i] == IoError::WriteError {
            validation_map[spot_blocks[i].idx] = BlockReport::WriteError;
        }
    }

    println!(
        "{}",
        console::style("\nReading blocks with random data").bold()
    );
    let read_random_blocks = read_blocks(
        drive.deref_mut(),
        &spot_blocks,
        cli.block_size_kb as usize * 1024,
    );

    // Fill the validation map.
    for i in 0..cli.num_blocks {
        if random_blocks.errors[i] == IoError::WriteError {
            validation_map[spot_blocks[i].idx] = BlockReport::WriteError;
        } else if read_random_blocks.errors[i] == IoError::ReadError {
            validation_map[spot_blocks[i].idx] = BlockReport::ReadError;
        } else if read_random_blocks.block(i) == random_blocks.block(i) {
            validation_map[spot_blocks[i].idx] = BlockReport::Validated;
        } else {
            validation_map[spot_blocks[i].idx] = BlockReport::NoStorage;
        }
    }

    print_validation_map(&validation_map, cli.map_width);

    // Find highest validated block (where all previous blocks are also validated).
    let mut highest_validated_block_idx = -1;
    for (i, v) in validation_map.iter().enumerate() {
        if *v != BlockReport::Validated {
            break;
        }
        highest_validated_block_idx = i as i64;
    }
    let mut validated_drive_size = 0;
    if highest_validated_block_idx >= 0 {
        for b in spot_blocks.iter() {
            if b.idx == highest_validated_block_idx as usize {
                // The validated drive size is the equal to the end of this block,
                // i.e. the beginning offset of the following block.
                validated_drive_size = (b.num + 1) * cli.block_size_kb * 1024;
                break;
            }
        }
    }
    println!(
        "{}: {} bytes ({:.3} GiB, {:.3} GB)",
        console::style("Validated drive size").bold(),
        validated_drive_size,
        validated_drive_size as f64 / 1024.0 / 1024.0 / 1024.0,
        validated_drive_size as f64 / 1000_000_000.0
    );

    if let Some(mut orig_data) = orig_data_option {
        println!("{}", console::style("\nWriting original blocks").bold());
        write_blocks(drive.deref_mut(), &spot_blocks, &mut orig_data);
    }
    Ok(())
}
