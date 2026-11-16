//! The comparability gate: read an archive's embedded provenance block and
//! decide whether a fresh archive is comparable to the committed baseline.
//!
//! Per the redesign the *schema* stays in elivagar (it authors the block); the
//! *gating policy* - which fields gate comparability and which are diagnostic -
//! is adjudication policy and lives here in brokkr (`elivagar.md`, "What
//! elivagar sheds"). The policy is applied **symmetrically at compare time**:
//! `contract_diff` runs brokkr's current partition over both the stored contract
//! and the fresh block, so a policy change never re-interprets one side only
//! (`brokkr.md`, "The gating policy").

use serde_json::Value;

/// Schema version of the `elivagar` metadata member brokkr understands.
pub const SCHEMA_VERSION: u64 = 1;

/// The archive contract consumed by the gate. Build provenance is kept whole for
/// diagnosis and rotation review, never comparison gating.
#[derive(Debug, Clone)]
pub struct ContractDoc {
    pub input: Value,
    pub config: Value,
    pub build: Value,
}

/// Outcome of reading the provenance block. Everything but `Contract` is an
/// archive-side refusal (exit 2) - the archive cannot be judged on its own terms.
///
/// The non-`Contract` variants and their payloads are surfaced through
/// `Display` in the refusal message.
#[derive(Debug, Clone)]
pub enum ContractState {
    Contract(ContractDoc),
    Absent,
    Invalid,
    UnknownSchema(u64),
    Incomplete(&'static str),
}

impl std::fmt::Display for ContractState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(_) => f.write_str("valid contract"),
            Self::Absent => f.write_str("no provenance block"),
            Self::Invalid => f.write_str("provenance block is not a valid object"),
            Self::UnknownSchema(schema) => write!(f, "unknown schema version {schema}"),
            Self::Incomplete(member) => write!(f, "provenance block lacks `{member}`"),
        }
    }
}

/// Decode the `elivagar` provenance member out of an archive's metadata JSON.
#[must_use]
pub fn extract_contract(metadata_json: &str) -> ContractState {
    let metadata: Value = match serde_json::from_str(metadata_json) {
        Ok(value) => value,
        Err(_) => return ContractState::Invalid,
    };
    let Some(elivagar) = metadata.get("elivagar") else {
        return ContractState::Absent;
    };
    let Some(object) = elivagar.as_object() else {
        return ContractState::Invalid;
    };
    let Some(schema) = object.get("schema").and_then(Value::as_u64) else {
        return ContractState::Incomplete("schema");
    };
    if schema != SCHEMA_VERSION {
        return ContractState::UnknownSchema(schema);
    }
    let Some(input) = object.get("input") else {
        return ContractState::Incomplete("input");
    };
    let Some(config) = object.get("config") else {
        return ContractState::Incomplete("config");
    };
    let Some(build) = object.get("build") else {
        return ContractState::Incomplete("build");
    };
    ContractState::Contract(ContractDoc {
        input: input.clone(),
        config: config.clone(),
        build: build.clone(),
    })
}

/// The gated paths which differ. `input.name` and the whole `build` block are
/// intentionally diagnostic: archives from different revisions are
/// exactly what the gate is meant to compare, so they must never gate.
#[must_use]
pub fn contract_diff(baseline: &ContractDoc, candidate: &ContractDoc) -> Vec<String> {
    let mut out = Vec::new();
    diff_json("config", &baseline.config, &candidate.config, &mut out);
    let mut left = baseline.input.clone();
    let mut right = candidate.input.clone();
    if let Some(value) = left.as_object_mut() {
        value.remove("name");
    }
    if let Some(value) = right.as_object_mut() {
        value.remove("name");
    }
    diff_json("input", &left, &right, &mut out);
    out
}

fn diff_json(path: &str, left: &Value, right: &Value, out: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<_> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                diff_json(
                    &format!("{path}.{key}"),
                    a.get(key).unwrap_or(&Value::Null),
                    b.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        _ if left != right => out.push(path.to_string()),
        _ => {}
    }
}

/// The diagnostic input name, warned on but never gated.
#[must_use]
pub fn input_name(c: &ContractDoc) -> Option<&str> {
    c.input.get("name").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    //! Ported from elivagar's `provenance` tests - the gating policy is brokkr's
    //! now, so its tests live with it.
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    fn contract_metadata() -> String {
        json!({
            "elivagar": {
                "schema": SCHEMA_VERSION,
                "input": {"name": "denmark", "xxh3_128": "ab", "features": {"locations_on_ways": true}},
                "config": {"ocean": {"artifact_key": {"policy_version": 2}}, "fanout_cap": 8},
                "build": {"elivagar": {"commit": "abc", "dirty": false}}
            }
        })
        .to_string()
    }

    #[test]
    fn extract_contract_names_every_no_contract_case() {
        assert!(matches!(
            extract_contract("not json"),
            ContractState::Invalid
        ));
        assert!(matches!(extract_contract("{}"), ContractState::Absent));
        assert!(matches!(
            extract_contract(r#"{"elivagar": 7}"#),
            ContractState::Invalid
        ));
        assert!(matches!(
            extract_contract(r#"{"elivagar": {}}"#),
            ContractState::Incomplete("schema")
        ));
        assert!(matches!(
            extract_contract(r#"{"elivagar": {"schema": 999}}"#),
            ContractState::UnknownSchema(999)
        ));
        assert!(matches!(
            extract_contract(r#"{"elivagar": {"schema": 1, "config": {}, "build": {}}}"#),
            ContractState::Incomplete("input")
        ));
        assert!(matches!(
            extract_contract(&contract_metadata()),
            ContractState::Contract(_)
        ));
    }

    #[test]
    fn contract_diff_gates_config_and_input_but_never_name_or_build() {
        let ContractState::Contract(base) = extract_contract(&contract_metadata()) else {
            panic!("baseline must parse");
        };
        // Identical contract: no diffs.
        assert!(contract_diff(&base, &base).is_empty());

        // A name-only change and a build-only change are both ignored (diagnostic).
        let mut named = base.clone();
        named.input["name"] = json!("germany");
        named.build["elivagar"]["commit"] = json!("def");
        assert!(contract_diff(&base, &named).is_empty());

        // The ocean policy version is gated and named by path.
        let mut policy = base.clone();
        policy.config["ocean"]["artifact_key"]["policy_version"] = json!(1);
        assert_eq!(
            contract_diff(&base, &policy),
            vec!["config.ocean.artifact_key.policy_version".to_string()]
        );
    }
}
