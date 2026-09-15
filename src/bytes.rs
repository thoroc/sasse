//! A size in bytes, written the way people write sizes.
//!
//! Configuration is edited by hand, so `log_budget = "200MB"` has to work.
//! Accepting a bare integer as well keeps the setting precise when that matters.

use std::fmt;

use eyre::{Result, eyre};
use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(u64);

/// Suffixes are powers of 1024, which is what someone writing "200MB" about
/// disk almost always means. Spelled out in the README so it is not a guess.
const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;

impl ByteSize {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn bytes(self) -> u64 {
        self.0
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let text = raw.trim();
        if text.is_empty() {
            return Err(eyre!("a size cannot be empty"));
        }

        let digits_end = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (number, suffix) = text.split_at(digits_end);

        if number.is_empty() {
            return Err(eyre!("{raw:?} does not start with a number"));
        }

        let multiplier = match suffix.trim().to_ascii_uppercase().as_str() {
            "" | "B" => 1,
            "K" | "KB" | "KIB" => KB,
            "M" | "MB" | "MIB" => MB,
            "G" | "GB" | "GIB" => GB,
            other => return Err(eyre!("{other:?} is not a size suffix I know")),
        };

        let value: u64 = number
            .parse()
            .map_err(|_| eyre!("{number:?} is not a whole number"))?;

        value
            .checked_mul(multiplier)
            .map(Self)
            .ok_or_else(|| eyre!("{raw:?} is too large to be a size in bytes"))
    }
}

impl fmt::Display for ByteSize {
    /// Written in the largest unit that divides exactly, and in bytes
    /// otherwise, so anything printed here can be pasted straight back into a
    /// config file. A fractional form would read better and would not reparse.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.0;
        for (unit, suffix) in [(GB, "GB"), (MB, "MB"), (KB, "KB")] {
            if bytes >= unit && bytes.is_multiple_of(unit) {
                return write!(f, "{}{suffix}", bytes / unit);
            }
        }
        write!(f, "{bytes}B")
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Written {
            Bytes(u64),
            Spelled(String),
        }

        match Written::deserialize(deserializer)? {
            Written::Bytes(bytes) => Ok(Self(bytes)),
            Written::Spelled(text) => Self::parse(&text).map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_number_is_bytes() {
        assert_eq!(ByteSize::parse("4096").unwrap().bytes(), 4096);
        assert_eq!(ByteSize::parse("512B").unwrap().bytes(), 512);
    }

    #[test]
    fn suffixes_are_powers_of_1024() {
        assert_eq!(ByteSize::parse("1KB").unwrap().bytes(), 1024);
        assert_eq!(ByteSize::parse("2MB").unwrap().bytes(), 2 * 1024 * 1024);
        assert_eq!(ByteSize::parse("1GB").unwrap().bytes(), 1024 * 1024 * 1024);
    }

    #[test]
    fn spelling_and_spacing_are_forgiving() {
        for spelling in ["200MB", "200mb", "200 MB", " 200MiB ", "200M"] {
            assert_eq!(
                ByteSize::parse(spelling).unwrap().bytes(),
                200 * 1024 * 1024,
                "{spelling} should be 200MB"
            );
        }
    }

    #[test]
    fn nonsense_is_rejected_rather_than_guessed() {
        for bad in ["", "   ", "MB", "twenty", "10 parsecs", "1.5MB", "-1"] {
            assert!(ByteSize::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    /// A size that overflows must not silently wrap into a tiny budget.
    #[test]
    fn an_unrepresentable_size_is_an_error() {
        assert!(ByteSize::parse("99999999999999999999GB").is_err());
        assert!(ByteSize::parse(&format!("{}GB", u64::MAX)).is_err());
    }

    #[test]
    fn display_uses_the_largest_exact_unit() {
        for (bytes, spelled) in [
            (512u64, "512B"),
            (1024, "1KB"),
            (4096, "4KB"),
            (2 * 1024 * 1024, "2MB"),
            (1024 * 1024 * 1024, "1GB"),
        ] {
            assert_eq!(ByteSize::new(bytes).to_string(), spelled);
        }
    }

    /// A size that is not a whole multiple is shown in bytes rather than
    /// rounded, because a rounded form would not parse back.
    #[test]
    fn an_inexact_size_is_shown_in_bytes() {
        assert_eq!(ByteSize::new(1536).to_string(), "1536B");
        assert_eq!(ByteSize::new(3 * 1024 * 1024 + 1).to_string(), "3145729B");
    }

    /// Anything sasse prints has to be something sasse accepts, or a size it
    /// reported cannot be pasted into a config file.
    #[test]
    fn every_displayed_size_parses_back_to_itself() {
        let interesting = [
            0u64,
            1,
            512,
            1023,
            1024,
            1025,
            4096,
            1536,
            MB - 1,
            MB,
            MB + 1,
            200 * MB,
            GB,
            3 * GB + 7,
            u64::MAX,
        ];

        for bytes in interesting {
            let size = ByteSize::new(bytes);
            let written = size.to_string();
            assert_eq!(
                ByteSize::parse(&written).unwrap(),
                size,
                "{bytes} displayed as {written} did not parse back"
            );
        }
    }

    #[test]
    fn both_written_forms_deserialize() {
        #[derive(Deserialize)]
        struct Settings {
            spelled: ByteSize,
            counted: ByteSize,
        }

        let settings: Settings = toml::from_str("spelled = \"2MB\"\ncounted = 2097152").unwrap();
        assert_eq!(settings.spelled, settings.counted);
    }
}
