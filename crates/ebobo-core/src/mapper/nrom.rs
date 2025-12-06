//! NROM (mapper 0): fixed 16/32 KiB PRG, 8 KiB CHR.

use super::{BoardLogic, Mem};

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Nrom;

impl BoardLogic for Nrom {
    #[inline]
    fn prg_index(&self, a: usize, _: &Mem) -> usize {
        a
    }
    #[inline]
    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        a
    }
    fn write(&mut self, _: u16, _: u8, _: u64, _: &Mem) {}
}

#[cfg(test)]
mod tests {
    use super::super::test_util::info;
    use super::super::Cartridge;

    #[test]
    fn mapper_nrom_mirrors_16k() {
        let mut c = Cartridge::new(&info(0, 1, 2)).unwrap();
        assert_eq!(c.cpu_read(0x8000), Some(0));
        assert_eq!(c.cpu_read(0xC000), Some(0));
        c.cpu_write(0x6000, 0x42, 0);
        assert_eq!(c.cpu_peek(0x6000), Some(0x42));
        assert_eq!(c.battery_ram().unwrap()[0], 0x42);
        assert_eq!(c.cpu_peek(0x5000), None);
    }
}
