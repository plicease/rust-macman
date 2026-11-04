//! What the wizard can see in an image, read without changing it, and what
//! it guesses from that. Facts and guesses are kept apart, and nothing here
//! looks at the file's name: names like `.toast` and `.img` promise nothing.

use crate::disk::{self, Classification, DiskFormat};
use crate::media::{self, MediaFormat, MediaKind};
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MDB: u64 = 1024;
/// `drVN`, the volume name, in an MFS or HFS master directory block.
const VOLUME_NAME: u64 = 0x24;
/// `drFndrInfo[0]` in an HFS master directory block: the blessed System Folder.
const BLESSED: u64 = 0x5C;
/// The volume identifier of an ISO 9660 primary volume descriptor.
const ISO_LABEL: u64 = 0x8028;

/// One classic volume found in an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Its partition, in a partitioned image.
    pub partition: Option<u32>,
    pub format: DiskFormat,
    pub name: String,
    pub boot_blocks: bool,
    /// Whether it names a System Folder to start from, as HFS does. MFS has
    /// no such field, so it's always false there.
    pub blessed: bool,
}

/// What an image was seen to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub path: PathBuf,
    pub size: u64,
    /// How `macman media add` would take it, or why it wouldn't.
    pub media: std::result::Result<(MediaKind, MediaFormat), String>,
    /// What `macman disk info` would call it.
    pub layout: String,
    pub volumes: Vec<Volume>,
    /// An ISO 9660 volume's identifier.
    pub iso_label: Option<String>,
    /// The partition map's partition types, in order.
    pub partition_kinds: Vec<String>,
    /// Whether `run --cdrom` could attach it, or why not.
    pub cd: std::result::Result<(), String>,
    /// Whether `create` could take its volume as a boot disk, or why not.
    pub volume_import: std::result::Result<(), String>,
}

impl Observation {
    /// Whether any volume has boot blocks and, for HFS, a blessed System Folder.
    pub fn startable(&self) -> bool {
        self.volumes.iter().any(|volume| {
            volume.boot_blocks && (volume.blessed || volume.format != DiskFormat::RawHfs)
        })
    }

    /// Whether it's laid out as CDs are: ISO 9660, or a partition map with
    /// a CD driver or Apple's CD scheme in it. Hard disks have neither.
    pub fn cd_layout(&self) -> bool {
        self.iso_label.is_some()
            || self
                .partition_kinds
                .iter()
                .any(|kind| kind.contains("_CD") || kind.starts_with("CD_"))
    }

    pub fn kind(&self) -> Option<MediaKind> {
        self.media.as_ref().ok().map(|(kind, _)| *kind)
    }

    /// The facts, one line each, as the wizard lists them.
    pub fn facts(&self) -> Vec<String> {
        let mut facts = vec![
            format!("File: {}", self.path.display()),
            format!("Size: {}", size_text(self.size)),
            format!("Layout: {}", self.layout),
        ];
        match &self.media {
            Ok((kind, format)) => facts.push(format!(
                "As media: a {} ({})",
                kind.as_str(),
                format.describe()
            )),
            Err(why) => facts.push(format!("As media: not usable ({why})")),
        }
        for volume in &self.volumes {
            let place = match volume.partition {
                Some(index) => format!("partition {index}: "),
                None => String::new(),
            };
            let name = if volume.name.is_empty() {
                "no name".to_string()
            } else {
                format!("{:?}", volume.name)
            };
            let boot = match (volume.boot_blocks, volume.blessed, volume.format) {
                (true, true, _) => "boot blocks, blessed System Folder",
                (true, false, DiskFormat::RawHfs) => "boot blocks, no blessed System Folder",
                (true, false, _) => "boot blocks",
                (false, _, _) => "no boot blocks",
            };
            facts.push(format!(
                "Volume: {place}{} {name}, {boot}",
                disk::volume_name(volume.format)
            ));
        }
        if !self.partition_kinds.is_empty() {
            facts.push(format!("Partitions: {}", self.partition_kinds.join(", ")));
        }
        if let Some(label) = &self.iso_label {
            facts.push(format!("ISO 9660 volume: {label:?}"));
        }
        facts
    }
}

pub fn size_text(size: u64) -> String {
    const K: u64 = 1024;
    if size >= K * K * K && size.is_multiple_of(K * K * K / 10) {
        format!("{:.1}G ({size} bytes)", size as f64 / (K * K * K) as f64)
    } else if size >= K * K {
        format!("{:.1}M ({size} bytes)", size as f64 / (K * K) as f64)
    } else {
        format!("{}K ({size} bytes)", size / K)
    }
}

/// Look at `path`, reading only.
pub fn observe(path: &Path) -> Result<Observation> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    if !std::fs::metadata(path)?.is_file() {
        anyhow::bail!("{} isn't a file", path.display());
    }
    let classification = disk::classify(path)?;
    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut volumes = Vec::new();
    let mut partition_kinds = Vec::new();
    let layout = match &classification {
        Classification::Supported(DiskFormat::DiskCopy42) => {
            let (offset, length, format) = disk::disk_copy_contents(path)?;
            if let Some(format) = format {
                volumes.push(volume(&mut file, None, format, offset, offset + length)?);
            }
            "a Disk Copy 4.2 image".to_string()
        }
        Classification::Supported(format) => {
            volumes.push(volume(&mut file, None, *format, 0, size)?);
            format!("a bare {} volume", disk::volume_name(*format))
        }
        Classification::Partitioned(partitions) => {
            for partition in partitions {
                partition_kinds.push(mac_roman(partition.kind.as_bytes()));
                if let Some(format) = partition.volume {
                    volumes.push(volume(
                        &mut file,
                        Some(partition.index),
                        format,
                        partition.offset,
                        partition.offset + partition.length,
                    )?);
                }
            }
            format!(
                "an Apple partition map with {} partition(s)",
                partitions.len()
            )
        }
        Classification::Blank => "blank".to_string(),
        Classification::Unsupported(what) => what.clone(),
        Classification::Unrecognized => "not a format macman knows".to_string(),
    };
    let iso_label = if disk::is_iso9660(path)? {
        let mut label = [0u8; 32];
        read_at(&mut file, ISO_LABEL, &mut label)?;
        Some(String::from_utf8_lossy(&label).trim().to_string())
    } else {
        None
    };
    let layout = match (&classification, &iso_label) {
        (Classification::Blank | Classification::Unrecognized, Some(_)) => {
            "an ISO 9660 CD image".to_string()
        }
        _ => layout,
    };
    let media = media::inspect(path, None, false)
        .map(|(kind, format, _)| (kind, format))
        .map_err(|error| format!("{error:#}"));
    let cd = quiet_cd_check(path, &classification, iso_label.is_some());
    let volume_import = disk::plan_import(path, None, false)
        .map(drop)
        .map_err(|error| format!("{error:#}"));
    Ok(Observation {
        path: path.to_path_buf(),
        size,
        media,
        layout,
        volumes,
        iso_label,
        partition_kinds,
        cd,
        volume_import,
    })
}

/// What [`disk::check_cd_image`] decides, without its note on stderr, which
/// would land in the middle of the wizard's screen.
fn quiet_cd_check(
    path: &Path,
    classification: &Classification,
    iso: bool,
) -> std::result::Result<(), String> {
    let hfs = match classification {
        Classification::Supported(format) => *format == DiskFormat::RawHfs,
        Classification::Partitioned(partitions) => partitions
            .iter()
            .any(|partition| partition.volume == Some(DiskFormat::RawHfs)),
        _ => false,
    };
    if hfs || iso {
        Ok(())
    } else {
        disk::check_cd_image(path)
            .map(drop)
            .map_err(|error| format!("{error:#}"))
    }
}

fn read_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buffer)?;
    Ok(())
}

/// The volume whose first byte is at `start`, reading nothing at or past `end`.
fn volume(
    file: &mut File,
    partition: Option<u32>,
    format: DiskFormat,
    start: u64,
    end: u64,
) -> Result<Volume> {
    let mut boot = [0u8; 2];
    read_at(file, start, &mut boot)?;
    let mut name = [0u8; 28];
    let mut blessed = [0u8; 4];
    if start + MDB + BLESSED + 4 <= end {
        read_at(file, start + MDB + VOLUME_NAME, &mut name)?;
        read_at(file, start + MDB + BLESSED, &mut blessed)?;
    }
    let length = usize::from(name[0]).min(27);
    Ok(Volume {
        partition,
        format,
        name: mac_roman(&name[1..=length]),
        boot_blocks: &boot == b"LK",
        blessed: format == DiskFormat::RawHfs && blessed != [0; 4],
    })
}

/// Mac Roman, as far as volume names need: ASCII as it is, and the rest as a
/// stand-in, so nothing in a name can reach the terminal as a control code.
fn mac_roman(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&byte| match byte {
            0x20..=0x7E => char::from(byte),
            0xA5 => '•',
            0xA8 => '®',
            0xA9 => '©',
            0xAA => '™',
            _ => '?',
        })
        .collect()
}

/// How sure a guess is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// A classic Mac OS version, as `[7, 5, 3]`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub Vec<u32>);

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let parts: Option<Vec<u32>> = text.split('.').map(|part| part.parse().ok()).collect();
        let parts = parts?;
        (!parts.is_empty() && parts.len() <= 4).then_some(Self(parts))
    }

    /// Whether this is at least `other`, comparing only as many parts as
    /// this has, so "9" counts as 9.0.4 or later as well as earlier.
    pub fn at_least(&self, other: &[u32]) -> bool {
        let shared = self.0.len().min(other.len());
        self.0[..shared] >= other[..shared]
    }

    /// Whether this is at most `other`, as [`Version::at_least`].
    pub fn at_most(&self, other: &[u32]) -> bool {
        let shared = self.0.len().min(other.len());
        self.0[..shared] <= other[..shared]
    }

    pub fn major(&self) -> u32 {
        self.0[0]
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self.0.iter().map(u32::to_string).collect();
        f.write_str(&parts.join("."))
    }
}

/// Which system an image probably holds, and why that's thought.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsGuess {
    /// "System" or "Mac OS", as Apple named that version.
    pub family: &'static str,
    pub version: Version,
    /// Whether it looks like an installer rather than an installed system.
    pub installer: bool,
    pub confidence: Confidence,
    pub evidence: String,
}

impl OsGuess {
    pub fn label(&self) -> String {
        let version = &self.version;
        if version.0.len() == 1 {
            format!("{} {version}.x", self.family)
        } else {
            format!("{} {version}", self.family)
        }
    }
}

/// The system named in a volume name such as "Mac OS 9.0.4" or "System 7.5.3",
/// if any. Only names Apple used are believed: System 1 to 7.5, Mac OS 7.6 to 9.
pub fn version_in_name(name: &str) -> Option<(&'static str, Version)> {
    let words: Vec<&str> = name
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
        .filter(|word| !word.is_empty())
        .collect();
    for (index, word) in words.iter().enumerate() {
        let lower = word.to_ascii_lowercase();
        // "MacOS9" or "MacOS 9.0.4", as well as "Mac OS 9".
        let (family, rest) = if let Some(rest) = lower.strip_prefix("macos") {
            ("Mac OS", rest.to_string())
        } else if lower == "mac"
            && words.get(index + 1).map(|w| w.to_ascii_lowercase()) == Some("os".to_string())
        {
            ("Mac OS", words.get(index + 2).unwrap_or(&"").to_string())
        } else if lower == "system" {
            ("System", words.get(index + 1).unwrap_or(&"").to_string())
        } else {
            continue;
        };
        let rest = if rest.is_empty() && family == "Mac OS" && lower.starts_with("macos") {
            words.get(index + 1).unwrap_or(&"").to_string()
        } else {
            rest
        };
        let digits: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let Some(version) = Version::parse(digits.trim_end_matches('.')) else {
            continue;
        };
        let believed = match family {
            "System" => (1..=7).contains(&version.major()) && version.at_most(&[7, 5, 5]),
            _ => {
                (7..=9).contains(&version.major())
                    && (version.major() > 7 || version.at_least(&[7, 6]))
            }
        };
        if believed {
            return Some((family, version));
        }
    }
    None
}

/// What the volume names suggest the image holds. Nothing is ever guessed
/// from the file's own name, and no name gives more than medium confidence:
/// a volume can be called anything.
pub fn identify(observation: &Observation) -> Option<OsGuess> {
    let names = observation
        .volumes
        .iter()
        .map(|volume| volume.name.as_str())
        .chain(observation.iso_label.as_deref());
    for name in names {
        if let Some((family, version)) = version_in_name(name) {
            let lower = name.to_ascii_lowercase();
            let installer = lower.contains("install")
                || lower.split_whitespace().any(|word| word == "cd")
                || observation.cd_layout();
            return Some(OsGuess {
                family,
                version,
                installer,
                confidence: Confidence::Medium,
                evidence: format!("the volume is named {name:?}"),
            });
        }
    }
    None
}

/// What a version given to `media add` says, if it's one Apple used. It's
/// the user's word at registration, so it's medium confidence, as a volume
/// name is: never more.
pub fn from_registered_version(version: &str, observation: &Observation) -> Option<OsGuess> {
    let text = version.trim();
    // "Mac OS 9.0.4" as well as "9.0.4".
    let (family, version) = version_in_name(text).or_else(|| {
        let parsed = Version::parse(text)?;
        let family = if parsed.at_least(&[7, 6]) {
            "Mac OS"
        } else {
            "System"
        };
        version_in_name(&format!("{family} {parsed}"))
    })?;
    Some(OsGuess {
        family,
        version,
        installer: observation.cd_layout(),
        confidence: Confidence::Medium,
        evidence: format!("it was registered as version {text:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_come_from_names_apple_used() {
        let found = |name: &str| version_in_name(name).map(|(f, v)| format!("{f} {v}"));
        assert_eq!(found("Mac OS 9.0.4"), Some("Mac OS 9.0.4".into()));
        assert_eq!(found("MacOS9"), Some("Mac OS 9".into()));
        assert_eq!(found("Mac OS 8.6 CD"), Some("Mac OS 8.6".into()));
        assert_eq!(found("System 7.5.3"), Some("System 7.5.3".into()));
        assert_eq!(found("System 6.0.8 Disk"), Some("System 6.0.8".into()));
        assert_eq!(found("Install Mac OS 8.1"), Some("Mac OS 8.1".into()));
        // Not names any version had, or not versions at all.
        assert_eq!(found("System Tools"), None);
        assert_eq!(found("Mac OS X Install"), None);
        assert_eq!(found("Mac OS 10.2"), None);
        assert_eq!(found("System 8"), None);
        assert_eq!(found("Mac OS 7.5"), None);
        assert_eq!(found("Macintosh HD"), None);
        assert_eq!(found(""), None);
    }

    #[test]
    fn short_versions_compare_as_ranges() {
        let nine = Version::parse("9").unwrap();
        assert!(nine.at_least(&[9, 0, 4]) && nine.at_most(&[9, 0, 4]));
        let v = Version::parse("9.1").unwrap();
        assert!(!v.at_most(&[9, 0, 4]));
        assert!(Version::parse("7.5.3").unwrap().at_least(&[7, 5, 2]));
        assert!(!Version::parse("7.5.1").unwrap().at_least(&[7, 5, 2]));
        assert_eq!(Version::parse("7.x"), None);
    }

    #[test]
    fn a_registered_version_is_read_either_way() {
        let observation = Observation {
            path: PathBuf::from("x"),
            size: 0,
            media: Err(String::new()),
            layout: String::new(),
            volumes: Vec::new(),
            iso_label: None,
            partition_kinds: Vec::new(),
            cd: Ok(()),
            volume_import: Ok(()),
        };
        let label = |v: &str| from_registered_version(v, &observation).map(|g| g.label());
        assert_eq!(label("9.0.4").as_deref(), Some("Mac OS 9.0.4"));
        assert_eq!(label("Mac OS 9.0.4").as_deref(), Some("Mac OS 9.0.4"));
        assert_eq!(label("6.0.8").as_deref(), Some("System 6.0.8"));
        assert_eq!(label("my favourite"), None);
        assert_eq!(label("10.4"), None);
    }

    #[test]
    fn names_are_made_safe_to_show() {
        assert_eq!(mac_roman(b"Mac\x1b[2J\xAA"), "Mac?[2J™");
    }
}
