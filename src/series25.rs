//! Driver for 25-series SPI Flash and EEPROM chips.

use crate::{utils::HexSlice, Error};
use bitflags::bitflags;
use core::fmt;
use core::marker::PhantomData;
pub use core::task::Poll;
pub use embedded_hal::digital::OutputPin;
use embedded_hal::spi::Operation;
pub use embedded_hal::spi::SpiDevice;

/// 3-Byte JEDEC manufacturer and device identification.
pub struct Identification {
    /// Data collected
    /// - First byte is the manufacturer's ID code from eg JEDEC Publication No. 106AJ
    /// - The trailing bytes are a manufacturer-specific device ID.
    bytes: [u8; 3],

    /// The number of continuations that precede the main manufacturer ID
    continuations: u8,
}

impl Identification {
    /// Build an Identification from JEDEC ID bytes.
    pub fn from_jedec_id(buf: &[u8]) -> Identification {
        // Example response for Cypress part FM25V02A:
        // 7F 7F 7F 7F 7F 7F C2 22 08  (9 bytes)
        // 0x7F is a "continuation code", not part of the core manufacturer ID
        // 0xC2 is the company identifier for Cypress (Ramtron)

        // Find the end of the continuation bytes (0x7F)
        let mut start_idx = 0;
        for (i, item) in buf.iter().enumerate().take(buf.len() - 2) {
            if *item != 0x7F {
                start_idx = i;
                break;
            }
        }

        Self {
            bytes: [buf[start_idx], buf[start_idx + 1], buf[start_idx + 2]],
            continuations: start_idx as u8,
        }
    }

    /// The JEDEC manufacturer code for this chip.
    pub fn mfr_code(&self) -> u8 {
        self.bytes[0]
    }

    /// The manufacturer-specific device ID for this chip.
    pub fn device_id(&self) -> &[u8] {
        self.bytes[1..].as_ref()
    }

    /// Number of continuation codes in this chip ID.
    ///
    /// For example the ARM Ltd identifier is `7F 7F 7F 7F 3B` (5 bytes), so
    /// the continuation count is 4.
    pub fn continuation_count(&self) -> u8 {
        self.continuations
    }
}

impl fmt::Debug for Identification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Identification")
            .field(&HexSlice(self.bytes))
            .finish()
    }
}

#[allow(unused)] // TODO support more features
enum Opcode {
    /// Read the 8-bit legacy device ID.
    ReadDeviceId = 0xAB,
    /// Read the 8-bit manufacturer and device IDs.
    ReadMfDId = 0x90,
    /// Read 16-bit manufacturer ID and 8-bit device ID.
    ReadJedecId = 0x9F,
    /// Set the write enable latch.
    WriteEnable = 0x06,
    /// Clear the write enable latch.
    WriteDisable = 0x04,
    /// Read the 8-bit status register.
    ReadStatus = 0x05,
    /// Write the 8-bit status register. Not all bits are writeable.
    WriteStatus = 0x01,
    Read = 0x03,
    PageProg = 0x02, // directly writes to EEPROMs too
    SectorErase = 0x20,
    BlockErase = 0xD8,
    ChipErase = 0xC7,
}

bitflags! {
    /// Status register bits.
    pub struct Status: u8 {
        /// Erase or write in progress.
        const BUSY = 1 << 0;
        /// Status of the **W**rite **E**nable **L**atch.
        const WEL = 1 << 1;
        /// The 3 protection region bits.
        const PROT = 0b00011100;
        /// **S**tatus **R**egister **W**rite **D**isable bit.
        const SRWD = 1 << 7;
    }
}

/// Trait for defining the size of a flash.
pub trait FlashParameters {
    /// The page write size in bytes.
    const PAGE_SIZE: usize;
    /// The sector erase size in bytes.
    const SECTOR_SIZE: usize;
    /// The block erase size in bytes.
    const BLOCK_SIZE: usize;
    /// The total chip size in bytes.
    const CHIP_SIZE: usize;
}

/// Driver for 25-series SPI Flash chips.
///
/// # Type Parameters
///
/// * **`SPI`**: The SPI master to which the flash chip is attached.
#[derive(Debug)]
pub struct Flash<SPI, FlashParams>
where
    FlashParams: FlashParameters,
{
    spi: SPI,
    params: PhantomData<FlashParams>,
}

impl<SPI, FlashParams> Flash<SPI, FlashParams>
where
    SPI: SpiDevice<u8>,
    FlashParams: FlashParameters,
{
    /// Creates a new 26-series flash driver.
    ///
    /// # Parameters
    ///
    /// * **`spi`**: An SPI master. Must be configured to operate in the correct
    ///   mode for the device.
    pub fn init(spi: SPI, _params: FlashParams) -> Result<Flash<SPI, FlashParams>, Error<SPI>> {
        let mut this = Flash {
            spi,
            params: PhantomData,
        };

        // If the MCU is reset and an old operation is still ongoing, wait for it to finish.
        this.wait_done()?;

        Ok(this)
    }

    /// Get the size of a page which can be written.
    pub fn page_write_size(&self) -> usize {
        FlashParams::PAGE_SIZE
    }

    /// Get the size of a sector which can be erased.
    pub fn sector_erase_size(&self) -> usize {
        FlashParams::SECTOR_SIZE
    }

    /// Get the size of a block which can be erased.
    pub fn block_erase_size(&self) -> usize {
        FlashParams::BLOCK_SIZE
    }

    /// Get the size of the flash chip.
    pub fn chip_size(&self) -> usize {
        FlashParams::CHIP_SIZE
    }

    fn command_transfer(&mut self, bytes: &mut [u8]) -> Result<(), Error<SPI>> {
        self.spi.transfer_in_place(bytes).map_err(Error::Spi)
    }

    fn command_write(&mut self, bytes: &[u8]) -> Result<(), Error<SPI>> {
        self.spi.write(bytes).map_err(Error::Spi)
    }

    /// Reads the JEDEC manufacturer/device identification.
    pub fn read_jedec_id(&mut self) -> Result<Identification, Error<SPI>> {
        // Optimistically read 12 bytes, even though some identifiers will be shorter
        let mut buf: [u8; 12] = [0; 12];
        buf[0] = Opcode::ReadJedecId as u8;
        self.command_transfer(&mut buf)?;

        // Skip buf[0] (SPI read response byte)
        Ok(Identification::from_jedec_id(&buf[1..]))
    }

    /// Reads the status register.
    pub fn read_status(&mut self) -> Result<Status, Error<SPI>> {
        let mut buf = [Opcode::ReadStatus as u8, 0];
        self.command_transfer(&mut buf)?;

        Ok(Status::from_bits_truncate(buf[1]))
    }

    fn write_enable(&mut self) -> Result<(), Error<SPI>> {
        let cmd_buf = [Opcode::WriteEnable as u8];
        self.command_write(&cmd_buf)
    }

    pub fn wait_done(&mut self) -> Result<(), Error<SPI>> {
        while self.read_status()?.contains(Status::BUSY) {}
        Ok(())
    }

    pub fn poll_wait_done(&mut self) -> Poll<()> {
        // TODO: Consider changing this to a delay based pattern
        let status = self.read_status().unwrap_or(Status::BUSY);

        if status.contains(Status::BUSY) {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    /// Reads flash contents into `buf`, starting at `addr`.
    ///
    /// Note that `addr` is not fully decoded: Flash chips will typically only
    /// look at the lowest `N` bits needed to encode their size, which means
    /// that the contents are "mirrored" to addresses that are a multiple of the
    /// flash size. Only 24 bits of `addr` are transferred to the device in any
    /// case, limiting the maximum size of 25-series SPI flash chips to 16 MiB.
    ///
    /// # Parameters
    ///
    /// * `addr`: 24-bit address to start reading at.
    /// * `buf`: Destination buffer to fill.
    pub fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Error<SPI>> {
        // TODO what happens if `buf` is empty?

        let cmd_buf = [
            Opcode::Read as u8,
            (addr >> 16) as u8,
            (addr >> 8) as u8,
            addr as u8,
        ];

        self.spi
            .transaction(&mut [Operation::Write(&cmd_buf), Operation::Read(buf)])
            .map_err(Error::Spi)
    }

    /// Erases a sector from the memory chip.
    ///
    /// # Parameters
    /// * `addr`: The address to start erasing at. If the address is not on a sector boundary,
    ///   the lower bits can be ignored in order to make it fit.
    pub fn erase_sector(mut self, addr: u32) -> Result<(), Error<SPI>> {
        self.write_enable()?;

        let cmd_buf = [
            Opcode::SectorErase as u8,
            (addr >> 16) as u8,
            (addr >> 8) as u8,
            addr as u8,
        ];
        self.command_write(&cmd_buf)?;
        self.wait_done()
    }

    /// Erases a block from the memory chip.
    ///
    /// # Parameters
    /// * `addr`: The address to start erasing at. If the address is not on a block boundary,
    ///   the lower bits can be ignored in order to make it fit.
    pub fn erase_block(mut self, addr: u32) -> Result<(), Error<SPI>> {
        self.write_enable()?;

        let cmd_buf = [
            Opcode::BlockErase as u8,
            (addr >> 16) as u8,
            (addr >> 8) as u8,
            addr as u8,
        ];
        self.command_write(&cmd_buf)?;
        self.wait_done()
    }

    /// Writes bytes onto the memory chip. This method is supposed to assume that the sectors
    /// it is writing to have already been erased and should not do any erasing themselves.
    ///
    /// # Parameters
    /// * `addr`: The address to write to.
    /// * `data`: The bytes to write to `addr`, note that it will only take the lowest 256 bytes
    /// from the slice.
    pub fn write_bytes(mut self, addr: u32, data: &[u8]) -> Result<(), Error<SPI>> {
        self.write_enable()?;

        let cmd_buf = [
            Opcode::PageProg as u8,
            (addr >> 16) as u8,
            (addr >> 8) as u8,
            addr as u8,
        ];

        self.spi
            .transaction(&mut [
                Operation::Write(&cmd_buf),
                Operation::Write(&data[..256.min(data.len())]),
            ])
            .map_err(Error::Spi)?;

        self.wait_done()
    }

    /// Erases the memory chip fully.
    ///
    /// Warning: Full erase operations can take a significant amount of time.
    /// Check your device's datasheet for precise numbers.
    pub fn erase_all(mut self) -> Result<(), Error<SPI>> {
        self.write_enable()?;
        let cmd_buf = [Opcode::ChipErase as u8];
        self.command_write(&cmd_buf)?;
        self.wait_done()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_jedec_id() {
        let cypress_id_bytes = [0x81, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0xC2, 0x22, 0x08];
        let ident = Identification::from_jedec_id(&cypress_id_bytes);
        assert_eq!(0xC2, ident.mfr_code());
        assert_eq!(6, ident.continuation_count());
        let device_id = ident.device_id();
        assert_eq!(device_id[0], 0x22);
        assert_eq!(device_id[1], 0x08);
    }
}
