use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use xxhash_rust::xxh3::xxh3_128;

use crate::regress::DetailAttr;

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
    pub fn load(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        let file = toml::from_str(std::str::from_utf8(&bytes).map_err(io::Error::other)?)
            .map_err(io::Error::other)?;
        Ok(Self {
            file,
            hash: format!("{:032x}", xxh3_128(&bytes)),
        })
    }
    pub fn hash_hex(&self) -> &str {
        &self.hash
    }
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
                .any(|(key, value)| key.as_ref() == rule.key && value.matches_toml(&rule.value))
            {
                return overlay(base.paint.clone(), &rule.paint);
            }
        }
        base.paint.clone()
    }
    pub fn fallback() -> Paint {
        Paint {
            fill: Some("#ff00ff".into()),
            ..Paint::default()
        }
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
    use super::{Paint, Style, StyleFile};
    use crate::regress::DetailAttr;
    use std::sync::Arc;

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
