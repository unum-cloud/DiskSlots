//! The places DiskSlots reaches and the addresses naming them.
//!
//! An address is a path, a drive from a byte base, or an S3 prefix:
//!
//! ```text
//! ~/Data/sift-1b                     a folder or a file
//! /dev/nvme1n1@64GiB                 a drive from byte 64 GiB
//! /dev/nvme1n1@64GiB+16GiB           the 16 GiB of it from byte 64 GiB on
//! s3://bigger-ann/sift-1b/           an S3 prefix, or an object without the trailing slash
//! ```
//!
//! A base and a length are whole numbers of B, KiB, MiB, GiB or TiB, bytes when bare; a file whose
//! name ends in such a suffix takes an explicit `@0` after it.

use std::fmt;
use std::future::Future;
use std::ops::Range;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use crate::Error;

/// Parts one S3 multipart upload may hold, per the S3 multipart upload limits.
pub const S3_PARTS_MAX: u16 = 10_000;

/// Bytes every part of an S3 multipart upload but the last must hold, per the same limits.
pub const S3_PART_BYTES_MIN: u64 = 5 << 20;

const S3_SCHEME: &str = "s3://";

/// The units a byte count takes, each `2^BINARY_UNIT_SHIFT` times the one before.
const BINARY_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
const BINARY_UNIT_SHIFT: u32 = 10;

/// The digits a rounded size or count keeps.
pub const SIGNIFICANT_DIGITS: u32 = 3;

/// Where an address points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    /// A folder, a file or a drive.
    Path(PathBuf),
    /// An S3 prefix, or one object when the prefix is its whole key.
    Bucket(ObjectAddress),
}

/// Where on a drive a copy lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// At the base the address wrote after its `@`.
    Exact,
    /// On the first free run long enough, as an address without an `@` asks.
    FirstFree,
}

/// A location and the byte extent of it an address names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub location: Location,
    /// Where the extent's byte zero sits on the medium.
    pub base: u64,
    /// Bytes past the base the extent spans, zero meaning to the medium's end.
    pub limit: u64,
    pub placement: Placement,
}

/// One S3 object, or a prefix of keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectAddress {
    pub bucket: String,
    pub key: String,
}

/// Why a text is not an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressError {
    NoBucket,
    NoLocation,
    Base,
    Length,
    Home,
}

impl Address {
    /// All of `location`, from its first byte to its end.
    pub fn whole(location: Location) -> Address {
        Address {
            location,
            base: 0,
            limit: 0,
            placement: Placement::FirstFree,
        }
    }

    pub fn path(path: &Path) -> Address {
        Address::whole(Location::Path(path.to_path_buf()))
    }

    /// The bytes `extent` of the file or drive at `path`.
    pub fn extent(path: &Path, extent: Range<u64>) -> Address {
        Address {
            location: Location::Path(path.to_path_buf()),
            base: extent.start,
            limit: extent.end - extent.start,
            placement: Placement::Exact,
        }
    }
}

impl Location {
    /// The location `name`, one or more components, below this one; itself for an empty name.
    pub fn join(&self, name: &str) -> Location {
        match self {
            _ if name.is_empty() => self.clone(),
            Location::Path(path) => Location::Path(path.join(name)),
            Location::Bucket(object) => {
                let separator = match object.key.is_empty() || object.key.ends_with('/') {
                    true => "",
                    false => "/",
                };
                Location::Bucket(ObjectAddress {
                    bucket: object.bucket.clone(),
                    key: format!("{}{separator}{name}", object.key),
                })
            }
        }
    }

    /// The last component: a file or folder name, or a key's, else the bucket's.
    pub fn name(&self) -> String {
        match self {
            Location::Path(path) => path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            Location::Bucket(object) => match object.key.trim_end_matches('/').rsplit('/').next() {
                Some(name) if !name.is_empty() => name.to_owned(),
                _ => object.bucket.clone(),
            },
        }
    }
}

/// Parses any form the module documentation lists.
pub fn parse_address(text: &str) -> Result<Address, AddressError> {
    let text = text.trim_end();
    if let Some(rest) = text.strip_prefix(S3_SCHEME) {
        let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty() {
            return Err(AddressError::NoBucket);
        }
        return Ok(Address::whole(Location::Bucket(ObjectAddress {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
        })));
    }
    let (location, base, limit, placement) = match extent_start(text.as_bytes()) {
        None => (text, 0, 0, Placement::FirstFree),
        Some(at) => {
            let extent = &text[at + 1..];
            let (base, limit) = match extent.split_once('+') {
                Some((base, limit)) => (base, parse_byte_count(limit).ok_or(AddressError::Length)?),
                None => (extent, 0),
            };
            let base = parse_byte_count(base).ok_or(AddressError::Base)?;
            (&text[..at], base, limit, Placement::Exact)
        }
    };
    if location.is_empty() {
        return Err(AddressError::NoLocation);
    }
    Ok(Address {
        location: Location::Path(expand_home(location)?),
        base,
        limit,
        placement,
    })
}

/// Where the `@` opening an extent sits: the last one, when a digit follows it.
fn extent_start(text: &[u8]) -> Option<usize> {
    let at = text.iter().rposition(|&byte| byte == b'@')?;
    text.get(at + 1)
        .is_some_and(u8::is_ascii_digit)
        .then_some(at)
}

/// A whole decimal number of bytes with an optional binary unit, `None` past `u64` or on an unknown unit.
fn parse_byte_count(text: &str) -> Option<u64> {
    let digits = text
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(text.len());
    let count: u64 = text[..digits].parse().ok()?;
    let unit = match &text[digits..] {
        "" => 0,
        suffix => BINARY_UNITS.iter().position(|unit| *unit == suffix)?,
    };
    count.checked_mul(1 << (unit as u32 * BINARY_UNIT_SHIFT))
}

fn expand_home(text: &str) -> Result<PathBuf, AddressError> {
    let rest = match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest.trim_start_matches('/'),
        _ => return Ok(PathBuf::from(text)),
    };
    let home = std::env::var_os("HOME").ok_or(AddressError::Home)?;
    Ok(Path::new(&home).join(rest))
}

/// `bytes` in the largest unit that divides it, as addresses write it.
pub fn byte_count(bytes: u64) -> String {
    let unit = (1..BINARY_UNITS.len())
        .rev()
        .find(|&unit| bytes != 0 && bytes.trailing_zeros() >= unit as u32 * BINARY_UNIT_SHIFT)
        .unwrap_or(0);
    match unit {
        0 => bytes.to_string(),
        _ => format!(
            "{}{}",
            bytes >> (unit as u32 * BINARY_UNIT_SHIFT),
            BINARY_UNITS[unit]
        ),
    }
}

/// `bytes` to three significant digits in binary units, as the screens show sizes.
pub fn size(bytes: u64) -> String {
    let unit = (0..BINARY_UNITS.len())
        .rev()
        .find(|&unit| bytes >> (unit as u32 * BINARY_UNIT_SHIFT) != 0)
        .unwrap_or(0);
    let shift = unit as u32 * BINARY_UNIT_SHIFT;
    let value = bytes as f64 / (1u64 << shift) as f64;
    format!(
        "{} {}",
        rounded(value, bytes >> shift, unit),
        BINARY_UNITS[unit]
    )
}

/// `value` with as many decimals as three significant digits leave past `whole`, none in the
/// smallest unit.
pub fn rounded(value: f64, whole: u64, unit: usize) -> String {
    let digits = whole.checked_ilog10().map_or(1, |log| log + 1);
    let decimals = match unit {
        0 => 0,
        _ => SIGNIFICANT_DIGITS.saturating_sub(digits) as usize,
    };
    format!("{value:.decimals$}")
}

/// `path` with the home folder written as `~`.
pub fn home_relative(path: &Path) -> String {
    let home = std::env::var_os("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rest) if !home.is_empty() && rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) if !home.is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = match &self.location {
            Location::Bucket(object) => return object.fmt(formatter),
            Location::Path(path) => path,
        };
        formatter.write_str(&home_relative(path))?;
        let named_like_extent = extent_start(path.as_os_str().as_encoded_bytes()).is_some();
        if self.placement == Placement::Exact || named_like_extent {
            write!(formatter, "@{}", byte_count(self.base))?;
        }
        if self.limit != 0 {
            write!(formatter, "+{}", byte_count(self.limit))?;
        }
        Ok(())
    }
}

impl fmt::Display for ObjectAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.key.as_str() {
            "" => write!(formatter, "{S3_SCHEME}{}", self.bucket),
            key => write!(formatter, "{S3_SCHEME}{}/{key}", self.bucket),
        }
    }
}

impl fmt::Display for AddressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            AddressError::NoBucket => "names no bucket",
            AddressError::NoLocation => "names no location",
            AddressError::Base => "has an unreadable base",
            AddressError::Length => "has an unreadable length",
            AddressError::Home => "starts with ~ while HOME is unset",
        })
    }
}

impl std::error::Error for AddressError {}

/// One entry of the Places screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    Folder(PathBuf),
    File(PathBuf),
    Drive(PathBuf),
    Bucket(ObjectAddress),
    /// An NVMe controller bound to `vfio-pci`, out of the kernel's reach, by its sysfs device folder.
    Bound(PathBuf),
}

impl Place {
    /// The place holding `address`, by what sits at its path.
    pub fn of(address: &Address) -> Result<Place, Error> {
        let path = match &address.location {
            Location::Bucket(object) => return Ok(Place::Bucket(object.clone())),
            Location::Path(path) => path.clone(),
        };
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.file_type().is_block_device() => Ok(Place::Drive(path)),
            Ok(metadata) if metadata.is_dir() => Ok(Place::Folder(path)),
            Ok(_) => Ok(Place::File(path)),
            Err(source) => Err(Error::Io {
                address: address.clone(),
                source,
            }),
        }
    }
}

impl fmt::Display for Place {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Place::Folder(path) | Place::File(path) | Place::Drive(path) => {
                formatter.write_str(&home_relative(path))
            }
            Place::Bucket(object) => match object.key.trim_end_matches('/') {
                "" => write!(formatter, "{S3_SCHEME}{}", object.bucket),
                prefix => write!(formatter, "{S3_SCHEME}{}/{prefix}", object.bucket),
            },
            Place::Bound(device) => {
                let name = device.file_name().unwrap_or(device.as_os_str());
                write!(formatter, "vfio {}", name.display())
            }
        }
    }
}

/// An S3 client and the runtime driving it, with the default credential, profile and region
/// resolution of the AWS SDK, and an optional endpoint for S3-compatible stores.
pub struct S3 {
    runtime: tokio::runtime::Runtime,
    pub client: aws_sdk_s3::Client,
}

impl S3 {
    /// A client when any of `addresses` names a bucket.
    pub fn for_addresses<'address>(
        addresses: impl IntoIterator<Item = &'address Address>,
        endpoint: Option<&str>,
    ) -> Result<Option<S3>, Error> {
        let Some(address) = addresses
            .into_iter()
            .find(|address| matches!(address.location, Location::Bucket(_)))
        else {
            return Ok(None);
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|source| Error::Io {
                address: address.clone(),
                source,
            })?;
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(endpoint) = endpoint {
            loader = loader.endpoint_url(endpoint);
        }
        let config = runtime.block_on(loader.load());
        Ok(Some(S3 {
            client: aws_sdk_s3::Client::new(&config),
            runtime,
        }))
    }

    pub fn block_on<Output>(&self, future: impl Future<Output = Output>) -> Output {
        self.runtime.block_on(future)
    }

    /// The first `bytes` bytes of `object`, fewer when it is shorter.
    pub fn prefix(&self, object: &ObjectAddress, bytes: u64) -> Result<Vec<u8>, Error> {
        if bytes == 0 {
            return Ok(Vec::new());
        }
        let get = self
            .client
            .get_object()
            .bucket(&object.bucket)
            .key(&object.key)
            .range(format!("bytes=0-{}", bytes - 1));
        let output = self.block_on(get.send()).map_err(s3_failed(object))?;
        let body = self
            .block_on(output.body.collect())
            .map_err(|source| Error::Io {
                address: Address::whole(Location::Bucket(object.clone())),
                source: source.into(),
            })?;
        Ok(body.into_bytes().to_vec())
    }
}

pub fn s3_failed<Source: Into<aws_sdk_s3::Error>>(
    object: &ObjectAddress,
) -> impl FnOnce(Source) -> Error + '_ {
    move |source| Error::S3 {
        object: object.clone(),
        source: Box::new(source.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str, base: u64, limit: u64) -> Address {
        Address {
            location: Location::Path(text.into()),
            base,
            limit,
            placement: Placement::Exact,
        }
    }

    #[test]
    fn parses_drive_extents_file_suffixes_and_s3_prefixes() {
        let drive = parse_address("/dev/nvme1n1@64GiB+16GiB").unwrap();
        assert_eq!(drive, path("/dev/nvme1n1", 64 << 30, 16 << 30));
        assert_eq!(drive.to_string(), "/dev/nvme1n1@64GiB+16GiB");
        // `@0` lands at slot 0; no `@` lands on the first free run.
        let first = parse_address("/dev/nvme1n1@0").unwrap();
        assert_eq!(first, path("/dev/nvme1n1", 0, 0));
        assert_eq!(first.to_string(), "/dev/nvme1n1@0");
        let free = parse_address("/dev/nvme1n1").unwrap();
        assert_eq!(free, Address::path(Path::new("/dev/nvme1n1")));
        assert_eq!(free.to_string(), "/dev/nvme1n1");
        assert_eq!(
            parse_address("/dev/nvme1n1@4097").unwrap().to_string(),
            "/dev/nvme1n1@4097"
        );

        // Only the last `@` followed by a digit splits, so a name ending like an extent takes `@0`.
        let suffixed = parse_address("/data/backup@2GiB@0").unwrap();
        assert_eq!(suffixed, path("/data/backup@2GiB", 0, 0));
        assert_eq!(suffixed.to_string(), "/data/backup@2GiB@0");
        let whole = |text: &str| Address::path(Path::new(text));
        assert_eq!(
            parse_address("/data/user@host").unwrap(),
            whole("/data/user@host")
        );
        assert_eq!(parse_address("/data/a@+5").unwrap(), whole("/data/a@+5"));

        assert_eq!(parse_address("/dev/nvme1n1@12QiB"), Err(AddressError::Base));
        assert_eq!(
            parse_address("/dev/nvme1n1@1GiB+"),
            Err(AddressError::Length)
        );
        assert_eq!(
            parse_address("/dev/nvme1n1@1GiB++1"),
            Err(AddressError::Length)
        );
        assert_eq!(
            parse_address("/dev/nvme1n1@16777216TiB"),
            Err(AddressError::Base),
            "2^64 bytes overflows"
        );
        assert_eq!(
            parse_address("/dev/nvme1n1@16777215TiB").unwrap().base,
            u64::MAX << 40
        );
        assert_eq!(parse_address("@0"), Err(AddressError::NoLocation));

        let bucket = parse_address("s3://bigger-ann").unwrap();
        let object = |key: &str| ObjectAddress {
            bucket: "bigger-ann".into(),
            key: key.into(),
        };
        assert_eq!(bucket.location, Location::Bucket(object("")));
        assert_eq!(bucket.to_string(), "s3://bigger-ann");
        let key = "sift-1b/index@2GiB.safegraphs";
        let named = parse_address(&format!("s3://bigger-ann/{key}")).unwrap();
        assert_eq!(named, Address::whole(Location::Bucket(object(key))));
        assert_eq!(named.to_string(), format!("s3://bigger-ann/{key}"));
        assert_eq!(parse_address("s3:///key"), Err(AddressError::NoBucket));
    }
}
