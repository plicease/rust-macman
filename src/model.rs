//! Emulator-independent facts about Macintosh hardware.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Model {
    Twig43,
    Twiggy,
    Mac128K,
    Mac512Ke,
    Plus,
    Se,
    SeFdhd,
    Classic,
    Pb100,
    II,
    IIx,
}

pub const ALL_MODELS: [Model; 11] = [
    Model::Twig43,
    Model::Twiggy,
    Model::Mac128K,
    Model::Mac512Ke,
    Model::Plus,
    Model::Se,
    Model::SeFdhd,
    Model::Classic,
    Model::Pb100,
    Model::II,
    Model::IIx,
];

impl Model {
    pub fn name(self) -> &'static str {
        match self {
            Model::Twig43 => "twig43",
            Model::Twiggy => "twiggy",
            Model::Mac128K => "128k",
            Model::Mac512Ke => "512ke",
            Model::Plus => "plus",
            Model::Se => "se",
            Model::SeFdhd => "sefdhd",
            Model::Classic => "classic",
            Model::Pb100 => "pb100",
            Model::II => "ii",
            Model::IIx => "iix",
        }
    }

    /// Expected ROM file size in bytes.
    pub fn rom_size(self) -> u64 {
        match self {
            Model::Twig43 | Model::Twiggy | Model::Mac128K => 64 * 1024,
            Model::Mac512Ke | Model::Plus => 128 * 1024,
            Model::Se | Model::SeFdhd | Model::Pb100 | Model::II | Model::IIx => 256 * 1024,
            Model::Classic => 512 * 1024,
        }
    }

    /// Known ROM checksums. All but the Twig43 store theirs in their first 4 bytes.
    pub fn rom_checksums(self) -> &'static [u32] {
        match self {
            Model::Twig43 => &[0x27F4_E04B],
            Model::Twiggy => &[0x2884_371D],
            Model::Mac128K => &[0x28BA_61CE, 0x28BA_4E50],
            Model::Mac512Ke | Model::Plus => &[0x4D1E_EEE1, 0x4D1E_EAE1, 0x4D1F_8172],
            Model::Se => &[0xB2E3_62A8],
            Model::SeFdhd => &[0xB306_E171],
            Model::Classic => &[0xA49F_9914],
            Model::Pb100 => &[0x9664_5F9C],
            Model::II => &[0x9779_D2C4, 0x9722_1136],
            Model::IIx => &[0x9722_1136],
        }
    }

    /// Whether `rom` starts with a genuine ROM for this model. This is the test
    /// Mini vMac applies (`ROM_IsValid`): the sum of the image's 16-bit words
    /// must be a known one, and must match the header on models that store it.
    pub fn accepts_rom(self, rom: &[u8]) -> bool {
        // The Classic's 512K ROM only checksums its first half.
        let summed = match self {
            Model::Classic => 256 * 1024,
            _ => self.rom_size() as usize,
        };
        if (rom.len() as u64) < self.rom_size() {
            return false;
        }
        let sum = rom[4..summed]
            .as_chunks::<2>()
            .0
            .iter()
            .fold(0u32, |sum, word| {
                sum.wrapping_add(u32::from(u16::from_be_bytes(*word)))
            });
        let header = u32::from_be_bytes([rom[0], rom[1], rom[2], rom[3]]);
        self.rom_checksums().contains(&sum) && (self == Model::Twig43 || sum == header)
    }

    pub fn default_memory(self) -> &'static str {
        match self {
            Model::Twig43 | Model::Twiggy | Model::Mac128K => "128K",
            Model::Mac512Ke => "512K",
            Model::II | Model::IIx => "8M",
            Model::Plus | Model::Se | Model::SeFdhd | Model::Classic | Model::Pb100 => "4M",
        }
    }

    pub fn default_display(self) -> (u32, u32, u32) {
        match self {
            Model::II | Model::IIx => (640, 480, 8),
            Model::Pb100 => (640, 400, 1),
            _ => (512, 342, 1),
        }
    }

    /// Whether the model has a colour-capable video card.
    pub fn has_video_card(self) -> bool {
        matches!(self, Model::II | Model::IIx)
    }

    /// 64K ROMs predate HFS and can only read MFS volumes.
    pub fn reads_hfs(self) -> bool {
        self.rom_size() > 64 * 1024
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Model {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let lower = s.to_ascii_lowercase();
        for model in ALL_MODELS {
            if model.name() == lower {
                return Ok(model);
            }
        }
        let known: Vec<_> = ALL_MODELS.iter().map(|m| m.name()).collect();
        bail!(
            "unknown model {s:?}; supported models: {}",
            known.join(", ")
        )
    }
}

impl TryFrom<String> for Model {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<Model> for String {
    fn from(model: Model) -> String {
        model.name().to_string()
    }
}

/// `WIDTHxHEIGHTxDEPTH`, as accepted by `--display`.
pub fn parse_display(s: &str) -> Result<(u32, u32, u32)> {
    let parts: Vec<_> = s.split('x').map(str::parse::<u32>).collect();
    match parts.as_slice() {
        [Ok(w), Ok(h), Ok(d)] => Ok((*w, *h, *d)),
        _ => bail!("invalid display {s:?}; expected WIDTHxHEIGHTxDEPTH, for example 640x480x8"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_round_trip() {
        for model in ALL_MODELS {
            assert_eq!(model.name().parse::<Model>().unwrap(), model);
        }
        assert_eq!("SE".parse::<Model>().unwrap(), Model::Se);
        assert!("lisa".parse::<Model>().is_err());
    }

    /// A synthetic image with a valid checksum and no Apple code.
    fn rom_with(checksum: u32, size: usize, summed: usize) -> Vec<u8> {
        let mut rom = vec![0u8; size];
        rom[..4].copy_from_slice(&checksum.to_be_bytes());
        let mut remaining = checksum;
        for word in rom[4..summed].as_chunks_mut::<2>().0 {
            let value = remaining.min(0xFFFF) as u16;
            *word = value.to_be_bytes();
            remaining -= u32::from(value);
        }
        assert_eq!(remaining, 0);
        rom
    }

    #[test]
    fn rom_checks_follow_the_emulator() {
        let plus = rom_with(0x4D1F_8172, 128 * 1024, 128 * 1024);
        assert!(Model::Plus.accepts_rom(&plus));
        assert!(Model::Mac512Ke.accepts_rom(&plus));
        assert!(!Model::Se.accepts_rom(&plus), "short for the SE");
        assert!(
            !Model::Mac128K.accepts_rom(&plus),
            "first 64K sums differently"
        );

        // Correct header, damaged body.
        let mut damaged = plus.clone();
        damaged[1000] ^= 1;
        assert!(!Model::Plus.accepts_rom(&damaged));

        // Correct sum, wrong header.
        let mut mislabelled = plus.clone();
        mislabelled[3] ^= 1;
        assert!(!Model::Plus.accepts_rom(&mislabelled));

        // Trailing data is outside the checked region.
        let mut trailing = plus;
        trailing.extend_from_slice(&[0xAA; 7504]);
        assert!(Model::Plus.accepts_rom(&trailing));
    }

    #[test]
    fn classic_sums_only_the_first_256k() {
        let mut classic = rom_with(0xA49F_9914, 512 * 1024, 256 * 1024);
        classic[300 * 1024] = 0xFF;
        assert!(Model::Classic.accepts_rom(&classic));
        assert!(!Model::Classic.accepts_rom(&classic[..256 * 1024]));
    }

    #[test]
    fn twig43_stores_no_header_checksum() {
        let mut rom = rom_with(0x27F4_E04B, 64 * 1024, 64 * 1024);
        rom[..4].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        assert!(Model::Twig43.accepts_rom(&rom));
    }

    #[test]
    fn display_parsing() {
        assert_eq!(parse_display("640x480x8").unwrap(), (640, 480, 8));
        for bad in ["640x480", "axbxc", "640x480x8x1", ""] {
            assert!(parse_display(bad).is_err(), "{bad:?}");
        }
    }
}
