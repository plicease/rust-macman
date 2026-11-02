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

    /// Known ROM checksums; a Macintosh ROM stores its own in its first 4 bytes.
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

    #[test]
    fn display_parsing() {
        assert_eq!(parse_display("640x480x8").unwrap(), (640, 480, 8));
        for bad in ["640x480", "axbxc", "640x480x8x1", ""] {
            assert!(parse_display(bad).is_err(), "{bad:?}");
        }
    }
}
