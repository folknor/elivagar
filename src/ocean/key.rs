// Ocean artifact identity: the XXH3-128 digest type, shapefile path pairs,
// and the two keys built from them. `OceanSourceKey` gates resume reuse of
// computed ocean chunks; `OceanArtifactKey` adds the compression level and
// is the durable artifact invalidation key. Their JSON wire shapes are flat,
// field-ordered and carry explicit nulls (reference/metadata.md).

use super::policy::OCEAN_POLICY_VERSION;
use serde::{Deserialize, Serialize};

/// A canonical XXH3-128 digest: exactly 32 lowercase hexadecimal characters
/// whenever it is serialized or displayed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Xxh128(u128);

impl Xxh128 {
    pub const fn from_u128(value: u128) -> Self {
        Self(value)
    }

    pub const fn as_u128(self) -> u128 {
        self.0
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(xxhash_rust::xxh3::xxh3_128(bytes))
    }

    /// Stream a file through the shared digest implementation and return its
    /// byte length with the digest.
    pub fn hash_file(path: &std::path::Path) -> std::io::Result<(Self, u64)> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        let mut buf = vec![0_u8; 8 << 20];
        let mut len = 0_u64;
        loop {
            let read = file.read(&mut buf)?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            let read = u64::try_from(read)
                .map_err(|_| std::io::Error::other("read size does not fit a u64 byte count"))?;
            len = len
                .checked_add(read)
                .ok_or_else(|| std::io::Error::other("file length exceeds the u64 byte count"))?;
        }
        Ok((Self(hasher.digest128()), len))
    }
}

impl std::fmt::Display for Xxh128 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:032x}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseXxh128Error;

impl std::fmt::Display for ParseXxh128Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("XXH3-128 must be 32 lowercase hexadecimal characters")
    }
}

impl std::error::Error for ParseXxh128Error {}

impl std::str::FromStr for Xxh128 {
    type Err = ParseXxh128Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ParseXxh128Error);
        }
        u128::from_str_radix(value, 16)
            .map(Self)
            .map_err(|_| ParseXxh128Error)
    }
}

impl From<u128> for Xxh128 {
    fn from(value: u128) -> Self {
        Self::from_u128(value)
    }
}

impl Serialize for Xxh128 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Xxh128 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// The `.shp` path and its matching `.shx` index, derived together so callers
/// cannot accidentally hash or open a mismatched pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapefilePair {
    shp: std::path::PathBuf,
    shx: std::path::PathBuf,
}

impl ShapefilePair {
    pub fn from_shp(shp: &std::path::Path) -> Self {
        Self {
            shp: shp.to_path_buf(),
            shx: shp.with_extension("shx"),
        }
    }

    pub fn shp(&self) -> &std::path::Path {
        &self.shp
    }

    pub fn shx(&self) -> &std::path::Path {
        &self.shx
    }
}

/// Identity of the shapefile inputs behind computed ocean sort chunks.
///
/// Deliberately NOT `OceanArtifactKey`, which carries `compression_level`.
/// This key exists to answer one question - may a `--skip-to` resume reuse
/// the ocean chunks already on disk - so it covers exactly what the ocean
/// phase bakes into those chunks: the source files and the zoom span. Chunks
/// hold geometry records, not encoded tiles, so compression level cannot
/// change them and gating on it would refuse a resume that is legitimate
/// (`--compression-level` is an assemble-side setting, and the checkpoint
/// producer config excludes those by design).
///
/// Without this the resume contract had a hole: a checkpoint recorded ocean
/// mode as the bare string "computed", so chunks built from one shapefile
/// could be reused by a resume naming another, and the archive's metadata
/// would then describe inputs that produced none of its ocean geometry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OceanSourceKey {
    pub full_shp_xxh128: Xxh128,
    pub full_shx_xxh128: Xxh128,
    pub simplified_shp_xxh128: Option<Xxh128>,
    pub simplified_shx_xxh128: Option<Xxh128>,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub policy_version: u32,
}

// Use a newtype deserializer rather than delegating directly to Option:
// serde treats a missing field as None when deserialize_option is called.
// These wire fields must be present, with explicit null for absent inputs.
#[derive(Deserialize)]
struct RequiredOption<T>(Option<T>);

#[derive(Deserialize)]
struct OceanSourceKeyWire {
    full_shp_xxh128: Xxh128,
    full_shx_xxh128: Xxh128,
    simplified_shp_xxh128: RequiredOption<Xxh128>,
    simplified_shx_xxh128: RequiredOption<Xxh128>,
    min_zoom: u8,
    max_zoom: u8,
    policy_version: u32,
}

impl<'de> Deserialize<'de> for OceanSourceKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = OceanSourceKeyWire::deserialize(deserializer)?;
        Ok(Self {
            full_shp_xxh128: wire.full_shp_xxh128,
            full_shx_xxh128: wire.full_shx_xxh128,
            simplified_shp_xxh128: wire.simplified_shp_xxh128.0,
            simplified_shx_xxh128: wire.simplified_shx_xxh128.0,
            min_zoom: wire.min_zoom,
            max_zoom: wire.max_zoom,
            policy_version: wire.policy_version,
        })
    }
}

impl OceanSourceKey {
    pub fn from_inputs(
        full: &ShapefilePair,
        simplified: Option<&ShapefilePair>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> std::io::Result<Self> {
        let (full_shp_xxh128, _) = Xxh128::hash_file(full.shp())?;
        let (full_shx_xxh128, _) = Xxh128::hash_file(full.shx())?;
        let (simplified_shp_xxh128, simplified_shx_xxh128) = if let Some(pair) = simplified {
            let (shp, _) = Xxh128::hash_file(pair.shp())?;
            let (shx, _) = Xxh128::hash_file(pair.shx())?;
            (Some(shp), Some(shx))
        } else {
            (None, None)
        };
        Ok(Self {
            full_shp_xxh128,
            full_shx_xxh128,
            simplified_shp_xxh128,
            simplified_shx_xxh128,
            min_zoom,
            max_zoom,
            policy_version: OCEAN_POLICY_VERSION,
        })
    }

    pub(crate) fn from_hashes(
        full: OceanShapefileHashes,
        simplified: Option<OceanShapefileHashes>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> Self {
        Self {
            full_shp_xxh128: full.shp_xxh128,
            full_shx_xxh128: full.shx_xxh128,
            simplified_shp_xxh128: simplified.map(|hashes| hashes.shp_xxh128),
            simplified_shx_xxh128: simplified.map(|hashes| hashes.shx_xxh128),
            min_zoom,
            max_zoom,
            policy_version: OCEAN_POLICY_VERSION,
        }
    }

    pub(crate) fn full_shapefile_hashes(&self) -> OceanShapefileHashes {
        OceanShapefileHashes {
            shp_xxh128: self.full_shp_xxh128,
            shx_xxh128: self.full_shx_xxh128,
        }
    }

    pub(crate) fn simplified_shapefile_hashes(&self) -> Option<OceanShapefileHashes> {
        Some(OceanShapefileHashes {
            shp_xxh128: self.simplified_shp_xxh128?,
            shx_xxh128: self.simplified_shx_xxh128?,
        })
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("OceanSourceKey has an infallible JSON representation")
    }

    pub fn from_json(value: &serde_json::Value) -> std::io::Result<Self> {
        serde_json::from_value(value.clone())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// The durable artifact invalidation key, encoded under `ocean_artifact`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OceanArtifactKey {
    pub source: OceanSourceKey,
    pub compression_level: u32,
}

impl OceanArtifactKey {
    pub fn from_inputs(
        full: &ShapefilePair,
        simplified: Option<&ShapefilePair>,
        min_zoom: u8,
        max_zoom: u8,
        compression_level: u32,
    ) -> std::io::Result<Self> {
        Ok(Self {
            source: OceanSourceKey::from_inputs(full, simplified, min_zoom, max_zoom)?,
            compression_level,
        })
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("OceanArtifactKey has an infallible JSON representation")
    }

    pub fn json(&self) -> String {
        serde_json::to_string(self).expect("OceanArtifactKey has an infallible JSON representation")
    }

    pub fn from_json(value: &serde_json::Value) -> std::io::Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| std::io::Error::other("ocean_artifact must be an object"))?;
        for field in [
            "full_shp_xxh128",
            "full_shx_xxh128",
            "simplified_shp_xxh128",
            "simplified_shx_xxh128",
            "min_zoom",
            "max_zoom",
            "compression_level",
            "policy_version",
        ] {
            if !object.contains_key(field) {
                return Err(std::io::Error::other(format!(
                    "ocean_artifact missing {field}"
                )));
            }
        }
        serde_json::from_value(value.clone())
            .map_err(|e| std::io::Error::other(format!("invalid ocean_artifact: {e}")))
    }
}

impl std::ops::Deref for OceanArtifactKey {
    type Target = OceanSourceKey;

    fn deref(&self) -> &Self::Target {
        &self.source
    }
}

impl std::ops::DerefMut for OceanArtifactKey {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.source
    }
}

/// Flat durable wire shape. Keep this field order aligned with ocean metadata.
#[derive(Serialize)]
struct OceanArtifactKeyRef<'a> {
    full_shp_xxh128: &'a Xxh128,
    full_shx_xxh128: &'a Xxh128,
    simplified_shp_xxh128: &'a Option<Xxh128>,
    simplified_shx_xxh128: &'a Option<Xxh128>,
    min_zoom: u8,
    max_zoom: u8,
    compression_level: u32,
    policy_version: u32,
}

#[derive(Deserialize)]
struct OceanArtifactKeyWire {
    full_shp_xxh128: Xxh128,
    full_shx_xxh128: Xxh128,
    simplified_shp_xxh128: RequiredOption<Xxh128>,
    simplified_shx_xxh128: RequiredOption<Xxh128>,
    min_zoom: u8,
    max_zoom: u8,
    compression_level: u32,
    policy_version: u32,
}

impl Serialize for OceanArtifactKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        OceanArtifactKeyRef {
            full_shp_xxh128: &self.source.full_shp_xxh128,
            full_shx_xxh128: &self.source.full_shx_xxh128,
            simplified_shp_xxh128: &self.source.simplified_shp_xxh128,
            simplified_shx_xxh128: &self.source.simplified_shx_xxh128,
            min_zoom: self.source.min_zoom,
            max_zoom: self.source.max_zoom,
            compression_level: self.compression_level,
            policy_version: self.source.policy_version,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OceanArtifactKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = OceanArtifactKeyWire::deserialize(deserializer)?;
        Ok(Self {
            source: OceanSourceKey {
                full_shp_xxh128: wire.full_shp_xxh128,
                full_shx_xxh128: wire.full_shx_xxh128,
                simplified_shp_xxh128: wire.simplified_shp_xxh128.0,
                simplified_shx_xxh128: wire.simplified_shx_xxh128.0,
                min_zoom: wire.min_zoom,
                max_zoom: wire.max_zoom,
                policy_version: wire.policy_version,
            },
            compression_level: wire.compression_level,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OceanShapefileHashes {
    pub(crate) shp_xxh128: Xxh128,
    pub(crate) shx_xxh128: Xxh128,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocean_artifact_wire_order_and_nulls_stay_flat() {
        let key = OceanArtifactKey {
            source: OceanSourceKey {
                full_shp_xxh128: Xxh128::from_u128(1),
                full_shx_xxh128: Xxh128::from_u128(2),
                simplified_shp_xxh128: None,
                simplified_shx_xxh128: None,
                min_zoom: 0,
                max_zoom: 14,
                policy_version: OCEAN_POLICY_VERSION,
            },
            compression_level: 6,
        };
        let expected = concat!(
            r#"{"full_shp_xxh128":"00000000000000000000000000000001","#,
            r#""full_shx_xxh128":"00000000000000000000000000000002","#,
            r#""simplified_shp_xxh128":null,"simplified_shx_xxh128":null,"#,
            r#""min_zoom":0,"max_zoom":14,"compression_level":6,"policy_version":POLICY}"#,
        )
        .replace("POLICY", &OCEAN_POLICY_VERSION.to_string());
        assert_eq!(key.json(), expected);
    }

    #[test]
    fn ocean_artifact_hashes_require_32_lowercase_hex_characters() {
        let valid = serde_json::json!({
            "full_shp_xxh128": "0123456789abcdef0123456789abcdef",
            "full_shx_xxh128": "fedcba9876543210fedcba9876543210",
            "simplified_shp_xxh128": null,
            "simplified_shx_xxh128": null,
            "min_zoom": 0,
            "max_zoom": 14,
            "compression_level": 6,
            "policy_version": OCEAN_POLICY_VERSION,
        });
        OceanArtifactKey::from_json(&valid).expect("valid artifact key");
        for invalid in [
            "0123456789abcdef0123456789abcde",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdef0",
        ] {
            let mut value = valid.clone();
            value["full_shp_xxh128"] = serde_json::Value::String(invalid.to_string());
            assert!(OceanArtifactKey::from_json(&value).is_err());
        }
    }

    #[test]
    fn ocean_keys_require_explicit_nulls_for_absent_inputs() {
        let source = OceanSourceKey {
            full_shp_xxh128: Xxh128::from_u128(1),
            full_shx_xxh128: Xxh128::from_u128(2),
            simplified_shp_xxh128: None,
            simplified_shx_xxh128: None,
            min_zoom: 0,
            max_zoom: 14,
            policy_version: OCEAN_POLICY_VERSION,
        };
        let artifact = OceanArtifactKey {
            source: source.clone(),
            compression_level: 6,
        };
        for field in ["simplified_shp_xxh128", "simplified_shx_xxh128"] {
            let mut value = source.to_json();
            OceanSourceKey::from_json(&value).expect("explicit nulls are valid");
            value.as_object_mut().expect("object").remove(field);
            assert!(OceanSourceKey::from_json(&value).is_err());

            let mut value = artifact.to_json();
            let _: OceanArtifactKey = serde_json::from_value(value.clone())
                .expect("explicit nulls are valid through serde");
            value.as_object_mut().expect("object").remove(field);
            assert!(serde_json::from_value::<OceanArtifactKey>(value).is_err());
        }
    }
}
