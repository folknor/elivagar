//! The shared source-identity inputs, read from `build/source_inputs.toml`.
//!
//! The file is data, not code, so the pre-Cargo trigger
//! `scripts/source_identity.py` reads it with a real TOML parser (`tomllib`)
//! and this module reads the same bytes, embedded with `include_str!`, through
//! a std-only scanner: build.rs takes no build dependency beyond xxhash-rust.
//! The scanner accepts only the subset the file needs and refuses everything
//! else, so a construct it cannot read fails the build instead of yielding a
//! silently different recipe. `tests/source_identity.rs` pins that the Python
//! reader reads exactly what this one reads.

/// The embedded file, the bytes both readers parse.
pub const SOURCE_INPUTS_TEXT: &str = include_str!("source_inputs.toml");

/// The shared inputs, as both the recipe and the trigger read them.
#[derive(Debug, PartialEq, Eq)]
pub struct SourceInputs {
    pub untracked_content_cap: u64,
    pub git_environment_prefix: String,
    pub git_metadata_directory: String,
    pub excluded_suffixes: Vec<String>,
    pub excluded_paths: Vec<String>,
    pub root_tree_exclusions: Vec<String>,
}

impl SourceInputs {
    /// The embedded file. Panics when it is malformed, which is inside the
    /// build script: the build fails loudly.
    pub fn embedded() -> Self {
        parse(SOURCE_INPUTS_TEXT)
            .unwrap_or_else(|cause| panic!("build/source_inputs.toml is malformed: {cause}"))
    }

    /// Whether a repository-relative path (forward slashes) is not a build
    /// input.
    pub fn excludes_path(&self, relative: &[u8]) -> bool {
        self.excluded_suffixes
            .iter()
            .any(|suffix| relative.ends_with(suffix.as_bytes()))
            || self
                .excluded_paths
                .iter()
                .any(|path| relative == path.as_bytes())
    }

    /// The same exclusions as git pathspecs, appended after `--` to every git
    /// read so git itself leaves those paths out of the diff and the untracked
    /// listing. The trigger builds the identical list. Every spec uses glob
    /// magic, so the excluded paths must carry no glob metacharacters; the
    /// file's paths carry none.
    pub fn git_pathspecs(&self) -> Vec<String> {
        let mut specs = vec![".".to_string()];
        specs.extend(
            self.excluded_suffixes
                .iter()
                .map(|suffix| format!(":(exclude,glob)**/*{suffix}")),
        );
        specs.extend(
            self.excluded_paths
                .iter()
                .map(|path| format!(":(exclude,glob){path}")),
        );
        specs
    }
}

#[derive(Debug)]
enum Value {
    Integer(u64),
    String(String),
    Strings(Vec<String>),
}

/// Read the shared file strictly: top-level `key = value` lines only, each key
/// exactly once, no unknown key, no duplicate entry in a list.
pub fn parse(text: &str) -> Result<SourceInputs, String> {
    let mut scanner = Scanner {
        bytes: text.as_bytes(),
        at: 0,
    };
    let mut fields: Vec<(String, Value)> = Vec::new();
    loop {
        scanner.skip_blank(true);
        let Some(byte) = scanner.peek() else { break };
        if byte == b'[' {
            return Err(format!("a table at byte {} is not allowed", scanner.at));
        }
        let key = scanner.key()?;
        scanner.skip_blank(false);
        scanner.expect(b'=')?;
        scanner.skip_blank(false);
        let value = scanner.value()?;
        scanner.skip_blank(false);
        match scanner.peek() {
            None | Some(b'\n') => {}
            Some(b'\r') if scanner.bytes.get(scanner.at + 1) == Some(&b'\n') => {}
            Some(_) => return Err(format!("trailing text after {key}")),
        }
        if fields.iter().any(|(seen, _)| *seen == key) {
            return Err(format!("key {key} is assigned twice"));
        }
        fields.push((key, value));
    }
    let mut take = |name: &str| {
        let index = fields
            .iter()
            .position(|(key, _)| key == name)
            .ok_or_else(|| format!("key {name} is missing"))?;
        Ok::<_, String>(fields.swap_remove(index).1)
    };
    let inputs = SourceInputs {
        untracked_content_cap: match take("untracked_content_cap")? {
            Value::Integer(value) => value,
            _ => return Err("untracked_content_cap must be an integer".to_string()),
        },
        git_environment_prefix: string(take("git_environment_prefix")?, "git_environment_prefix")?,
        git_metadata_directory: string(take("git_metadata_directory")?, "git_metadata_directory")?,
        excluded_suffixes: strings(take("excluded_suffixes")?, "excluded_suffixes")?,
        excluded_paths: strings(take("excluded_paths")?, "excluded_paths")?,
        root_tree_exclusions: strings(take("root_tree_exclusions")?, "root_tree_exclusions")?,
    };
    if let Some((key, _)) = fields.first() {
        return Err(format!("key {key} is not a shared source input"));
    }
    Ok(inputs)
}

fn string(value: Value, name: &str) -> Result<String, String> {
    match value {
        Value::String(value) => Ok(value),
        _ => Err(format!("{name} must be a string")),
    }
}

fn strings(value: Value, name: &str) -> Result<Vec<String>, String> {
    let Value::Strings(values) = value else {
        return Err(format!("{name} must be an array of strings"));
    };
    for (index, value) in values.iter().enumerate() {
        if values[..index].contains(value) {
            return Err(format!("{name} contains a duplicate entry"));
        }
    }
    Ok(values)
}

struct Scanner<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    /// Skip spaces, tabs and comments; newlines too when `newlines`.
    fn skip_blank(&mut self, newlines: bool) {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' => self.at += 1,
                b'\r' | b'\n' if newlines => self.at += 1,
                b'#' => {
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.at += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected {:?} at byte {}", byte as char, self.at))
        }
    }

    fn key(&mut self) -> Result<String, String> {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            self.at += 1;
        }
        if start == self.at {
            return Err(format!("expected a bare key at byte {start}"));
        }
        Ok(String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned())
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some(b'"' | b'\'') => self.string().map(Value::String),
            Some(b'[') => {
                self.at += 1;
                let mut values = Vec::new();
                loop {
                    self.skip_blank(true);
                    if self.peek() == Some(b']') {
                        self.at += 1;
                        return Ok(Value::Strings(values));
                    }
                    values.push(self.string()?);
                    self.skip_blank(true);
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b']') => {}
                        _ => return Err(format!("expected , or ] at byte {}", self.at)),
                    }
                }
            }
            Some(b'0'..=b'9') => {
                let start = self.at;
                while self.peek().is_some_and(|b| b.is_ascii_digit() || b == b'_') {
                    self.at += 1;
                }
                let digits = &self.bytes[start..self.at];
                // TOML: underscores only between digits, no leading zero.
                let well_formed = !digits.ends_with(b"_")
                    && !digits.windows(2).any(|w| w == b"__")
                    && (digits.len() == 1 || digits[0] != b'0');
                let text: String = digits
                    .iter()
                    .filter(|b| **b != b'_')
                    .map(|b| *b as char)
                    .collect();
                match (well_formed, text.parse()) {
                    (true, Ok(value)) => Ok(Value::Integer(value)),
                    _ => Err(format!("malformed integer at byte {start}")),
                }
            }
            _ => Err(format!("unsupported value at byte {}", self.at)),
        }
    }

    /// A single-line basic or literal string. Basic strings accept only the
    /// `\"` and `\\` escapes; anything else is refused rather than guessed.
    fn string(&mut self) -> Result<String, String> {
        let start = self.at;
        let quote = self.peek().ok_or("expected a string")?;
        if quote != b'"' && quote != b'\'' {
            return Err(format!("expected a string at byte {start}"));
        }
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let byte = self
                .peek()
                .ok_or_else(|| format!("unterminated string at byte {start}"))?;
            self.at += 1;
            match byte {
                b'\n' | b'\r' => return Err(format!("unterminated string at byte {start}")),
                b if b == quote => break,
                b'\\' if quote == b'"' => {
                    let escaped = self.peek();
                    self.at += 1;
                    match escaped {
                        Some(b @ (b'"' | b'\\')) => out.push(b),
                        _ => return Err(format!("unsupported escape at byte {}", self.at - 1)),
                    }
                }
                b => out.push(b),
            }
        }
        String::from_utf8(out).map_err(|_| format!("non-UTF-8 string at byte {start}"))
    }
}
