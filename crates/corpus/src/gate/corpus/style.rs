//! The canonical render style: `corpus/style.toml`. Owned by brokkr with the
//! render core (`elivagar.md`, "the style machinery"). Ported from elivagar's
//! `corpus/style.rs`; `matches_toml` becomes a free function since `DetailAttr`
//! is now elivagar's and carries no style logic.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use xxhash_rust::xxh3::xxh3_128;

use super::super::constants::SVG_FALLBACK_COLOR;
use super::super::eliv::DetailAttr;

#[derive(Deserialize)]
pub struct StyleFile {
    pub background: String,
    #[serde(default)]
    pub layer: Vec<StyleLayer>,
}

#[derive(Deserialize)]
pub struct StyleLayer {
    pub name: String,
    #[serde(default)]
    pub r#match: Vec<StyleMatch>,
    #[serde(flatten)]
    pub paint: Paint,
}

#[derive(Deserialize)]
pub struct StyleMatch {
    pub key: String,
    pub value: toml::Value,
    #[serde(flatten)]
    pub paint: Paint,
}

#[derive(Deserialize, Default, Clone)]
pub struct Paint {
    pub fill: Option<String>,
    pub fill_opacity: Option<String>,
    pub stroke: Option<String>,
    pub stroke_width: Option<String>,
    pub stroke_dasharray: Option<String>,
    pub stroke_opacity: Option<String>,
    pub point_radius: Option<u32>,
}

pub struct Style {
    pub file: StyleFile,
    hash: String,
}

impl Style {
    /// A malformed style is `InvalidData` (not `Other`), so the gate can fold it
    /// into the baseline verdict like any other damaged committed file.
    pub fn load(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let value: toml::Value =
            toml::from_str(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        validate_style_keys(&value)?;
        let file: StyleFile = value
            .try_into()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Self {
            file,
            hash: format!("{:032x}", xxh3_128(&bytes)),
        })
    }
    #[must_use]
    pub fn hash_hex(&self) -> &str {
        &self.hash
    }
    #[must_use]
    pub fn position(&self, layer: &str) -> Option<usize> {
        self.file.layer.iter().position(|v| v.name == layer)
    }
    pub(crate) fn resolve(&self, layer: &str, attrs: &[(Arc<str>, DetailAttr)]) -> Paint {
        let Some(base) = self.file.layer.iter().find(|v| v.name == layer) else {
            return Self::fallback();
        };
        for rule in &base.r#match {
            if attrs
                .iter()
                .any(|(key, value)| key.as_ref() == rule.key && matches_toml(value, &rule.value))
            {
                return overlay(base.paint.clone(), &rule.paint);
            }
        }
        base.paint.clone()
    }
    #[must_use]
    pub fn fallback() -> Paint {
        Paint {
            fill: Some(SVG_FALLBACK_COLOR.into()),
            ..Paint::default()
        }
    }
}

// Paint is flattened into both layers and matches; serde does not allow
// deny_unknown_fields on a flattened struct, so validate those maps here.
fn validate_style_keys(value: &toml::Value) -> io::Result<()> {
    const PAINT_FIELDS: &[&str] = &[
        "fill",
        "fill_opacity",
        "stroke",
        "stroke_width",
        "stroke_dasharray",
        "stroke_opacity",
        "point_radius",
    ];
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let root = value
        .as_table()
        .ok_or_else(|| invalid("style must be a TOML table".into()))?;
    reject_unknown_keys(root, &["background", "layer"], "style")?;
    if let Some(layers) = root.get("layer") {
        let layers = layers
            .as_array()
            .ok_or_else(|| invalid("style.layer must be an array of tables".into()))?;
        for (index, layer) in layers.iter().enumerate() {
            let layer = layer
                .as_table()
                .ok_or_else(|| invalid(format!("style.layer[{index}] must be a table")))?;
            let mut allowed = vec!["name", "match"];
            allowed.extend_from_slice(PAINT_FIELDS);
            reject_unknown_keys(layer, &allowed, &format!("style.layer[{index}]"))?;
            if let Some(matches) = layer.get("match") {
                let matches = matches.as_array().ok_or_else(|| {
                    invalid(format!(
                        "style.layer[{index}].match must be an array of tables"
                    ))
                })?;
                for (match_index, rule) in matches.iter().enumerate() {
                    let rule = rule.as_table().ok_or_else(|| {
                        invalid(format!(
                            "style.layer[{index}].match[{match_index}] must be a table"
                        ))
                    })?;
                    let mut allowed = vec!["key", "value"];
                    allowed.extend_from_slice(PAINT_FIELDS);
                    reject_unknown_keys(
                        rule,
                        &allowed,
                        &format!("style.layer[{index}].match[{match_index}]"),
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn reject_unknown_keys(table: &toml::Table, allowed: &[&str], context: &str) -> io::Result<()> {
    if let Some(key) = table.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown {context} key {key:?}"),
        ));
    }
    Ok(())
}

/// Whether a decoded attribute matches a style rule's TOML value.
fn matches_toml(attr: &DetailAttr, value: &toml::Value) -> bool {
    match (attr, value) {
        (DetailAttr::String(a), toml::Value::String(b)) => a.as_ref() == b,
        (DetailAttr::Int(a), toml::Value::Integer(b)) => *a == *b,
        (DetailAttr::UInt(a), toml::Value::Integer(b)) => u64::try_from(*b).ok() == Some(*a),
        (DetailAttr::SInt(a), toml::Value::Integer(b)) => *a == *b,
        (DetailAttr::Bool(a), toml::Value::Boolean(b)) => *a == *b,
        _ => false,
    }
}

fn overlay(mut base: Paint, over: &Paint) -> Paint {
    macro_rules! field {
        ($n:ident) => {
            if over.$n.is_some() {
                base.$n = over.$n.clone();
            }
        };
    }
    field!(fill);
    field!(fill_opacity);
    field!(stroke);
    field!(stroke_width);
    field!(stroke_dasharray);
    field!(stroke_opacity);
    if over.point_radius.is_some() {
        base.point_radius = over.point_radius;
    }
    base
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Arc;

    use super::super::super::eliv::DetailAttr;
    use super::{Paint, Style, StyleFile};

    fn style(text: &str) -> Style {
        Style {
            file: toml::from_str::<StyleFile>(text).expect("parse style"),
            hash: "0".to_string(),
        }
    }

    #[test]
    fn resolve_applies_first_matching_rule() {
        let s = style(
            "background = \"#fff\"\n[[layer]]\nname = \"streets\"\nstroke = \"#ddd\"\nstroke_width = \"1\"\n[[layer.match]]\nkey = \"kind\"\nvalue = \"motorway\"\nstroke = \"#e892a2\"\nstroke_width = \"6\"\n",
        );
        let attrs = vec![(Arc::from("kind"), DetailAttr::String(Arc::from("motorway")))];
        let paint: Paint = s.resolve("streets", &attrs);
        assert_eq!(paint.stroke.as_deref(), Some("#e892a2"));
        assert_eq!(paint.stroke_width.as_deref(), Some("6"));
    }

    #[test]
    fn resolve_falls_back_to_base_without_match() {
        let s = style(
            "background = \"#fff\"\n[[layer]]\nname = \"streets\"\nstroke = \"#ddd\"\n[[layer.match]]\nkey = \"kind\"\nvalue = \"motorway\"\nstroke = \"#e892a2\"\n",
        );
        let attrs = vec![(Arc::from("kind"), DetailAttr::String(Arc::from("service")))];
        assert_eq!(s.resolve("streets", &attrs).stroke.as_deref(), Some("#ddd"));
    }

    #[test]
    fn resolve_matches_integer_value() {
        let s = style(
            "background = \"#fff\"\n[[layer]]\nname = \"boundaries\"\nstroke_width = \"1\"\n[[layer.match]]\nkey = \"admin_level\"\nvalue = 2\nstroke_width = \"2\"\n",
        );
        let attrs = vec![(Arc::from("admin_level"), DetailAttr::Int(2))];
        assert_eq!(
            s.resolve("boundaries", &attrs).stroke_width.as_deref(),
            Some("2")
        );
    }

    #[test]
    fn unstyled_layer_uses_magenta_fallback() {
        let s = style("background = \"#fff\"\n");
        assert_eq!(s.resolve("nope", &[]).fill.as_deref(), Some("#ff00ff"));
        assert_eq!(s.position("nope"), None);
    }
}
