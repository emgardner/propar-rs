#![no_std]
#![forbid(unsafe_code)]
//! Allocation-free FlowDDE parameter definitions.
//!
//! The catalog follows the upstream 1.3.0 database, including its type overrides
//! for setpoint and analog input. Text parameters request NUL-terminated strings.
//! Binary strings with a specified length retain that fixed wire size.
//!
//! Numeric lookup is available without features. Enable `metadata` for parameter
//! names, name search, and documented values. Without it, the catalog contains no
//! descriptive strings or string pointers.

use propar_core::message::Parameter;

// Discard the name before code generation when metadata is disabled, including
// its pointer in each table row. Keep one source of truth for the definitions.
macro_rules! def {
    ($dde:expr, $name:literal, $process:expr, $number:expr, $kind:expr) => {
        $crate::Definition {
            dde: $dde,
            #[cfg(feature = "metadata")]
            name: $name,
            parameter: propar_core::message::Parameter::new($process, $number, $kind),
        }
    };
}

mod data;

/// A FlowDDE parameter and its channel-one ProPar address and type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Definition {
    pub dde: u16,
    /// Human-readable parameter name, available with `metadata`.
    #[cfg(feature = "metadata")]
    pub name: &'static str,
    pub parameter: Parameter,
}

/// One documented value or bitfield option for a FlowDDE parameter.
#[cfg(feature = "metadata")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValueDescription {
    /// Upstream database row ID.
    pub id: u16,
    pub dde: u16,
    /// Upstream short name of the parameter.
    pub name: &'static str,
    pub value: u8,
    /// Bit mask in upstream notation (e.g. `"&H01"`), or `""` for an enumerated value.
    pub filter: &'static str,
    pub description: &'static str,
}

#[cfg(feature = "metadata")]
const fn option(
    id: u16,
    dde: u16,
    name: &'static str,
    value: u8,
    filter: &'static str,
    description: &'static str,
) -> ValueDescription {
    ValueDescription {
        id,
        dde,
        name,
        value,
        filter,
        description,
    }
}

/// Iterate over the canonical definition for each DDE number in ascending order.
pub fn all() -> impl ExactSizeIterator<Item = &'static Definition> {
    data::DEFINITIONS.iter()
}

/// Look up a canonical parameter by its FlowDDE number.
pub fn by_dde(dde: u16) -> Option<&'static Definition> {
    let index = data::DEFINITIONS
        .binary_search_by_key(&dde, |p| p.dde)
        .ok()?;
    Some(&data::DEFINITIONS[index])
}

/// Find all possible definitions in a process, including legacy type aliases.
pub fn for_process(process: u8) -> impl Iterator<Item = &'static Definition> {
    data::ALIASES
        .iter()
        .chain(all())
        .filter(move |p| p.parameter.process == process)
}

/// Find every DDE definition that matches a ProPar address, including alternate types.
pub fn by_address(process: u8, number: u8) -> impl Iterator<Item = &'static Definition> {
    for_process(process).filter(move |p| p.parameter.number == number)
}

/// Search for a substring of the parameter name, ignoring ASCII case and spaces.
/// The iterator borrows the query and uses no additional storage.
#[cfg(feature = "metadata")]
pub fn search(query: &str) -> impl Iterator<Item = &'static Definition> + '_ {
    all().filter(move |p| matches_name(p.name, query))
}

#[cfg(feature = "metadata")]
fn matches_name(name: &str, query: &str) -> bool {
    fn normalized(text: &str) -> impl Iterator<Item = char> + Clone + '_ {
        text.chars()
            .filter(|&c| c != ' ')
            .map(|c| c.to_ascii_lowercase())
    }

    let query = normalized(query);
    let mut remaining = normalized(name);
    loop {
        let mut candidate = remaining.clone();
        if query.clone().all(|c| candidate.next() == Some(c)) {
            return true;
        }
        if remaining.next().is_none() {
            return false;
        }
    }
}

/// Enumerate documented values for a parameter in upstream order.
#[cfg(feature = "metadata")]
pub fn values(dde: u16) -> impl Iterator<Item = &'static ValueDescription> {
    data::VALUES.iter().filter(move |v| v.dde == dde)
}

/// Map a channel-one definition to a channel in `1..=16`.
///
/// Only processes 1, 33, 65, 97, and 104 are channel-relative. This function
/// expects a base parameter, so apply it once before sending a request.
pub fn for_channel(mut parameter: Parameter, channel: u8) -> Option<Parameter> {
    if !(1..=16).contains(&channel) {
        return None;
    }
    if matches!(parameter.process, 1 | 33 | 65 | 97 | 104) {
        parameter.process += channel - 1;
    }
    Some(parameter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use DataType::*;
    use propar_core::message::DataType;

    #[test]
    fn canonical_definitions_match_upstream_overrides() {
        assert_eq!(all().len(), 425);
        assert!(
            data::DEFINITIONS
                .windows(2)
                .all(|pair| pair[0].dde < pair[1].dde)
        );
        assert_eq!(by_dde(8).unwrap().parameter, Parameter::new(1, 0, Bht));
        assert_eq!(by_dde(9).unwrap().parameter.kind, U16);
        assert_eq!(by_dde(11).unwrap().parameter.kind, I16);
        assert_eq!(by_dde(52).unwrap().parameter.kind, F32);
        assert_eq!(by_dde(278).unwrap().parameter.kind, U32);
        assert_eq!(by_dde(298).unwrap().parameter.kind, F32);
        assert_eq!(by_dde(129).unwrap().parameter.kind, DataType::String(0));
        assert_eq!(by_dde(61).unwrap().parameter.kind, DataType::String(8));
        assert!(by_dde(0).is_none());
        for def in all() {
            assert!(by_address(def.parameter.process, def.parameter.number).any(|p| p == def));
        }
    }

    #[test]
    fn address_lookup_includes_every_applicable_definition() {
        assert!(by_address(114, 12).map(|p| p.parameter.kind).eq([U8, F32]));
        assert!(by_address(116, 29).map(|p| p.parameter.kind).eq([U32]));
    }

    #[cfg(feature = "metadata")]
    #[test]
    fn metadata_matches_upstream_names_and_values() {
        assert_eq!(data::VALUES.len(), 437);
        assert_eq!(by_dde(9).unwrap().name, "Setpoint");
        assert!(search("dSp Register").any(|p| p.dde == 278));
        assert_eq!(search("").count(), all().len());
        assert_eq!(values(12).count(), 25);
        assert!(values(175).any(|v| v.value == 41 && v.description == "DC: digital controller"));
        assert!(values(25535).next().is_none());
    }

    #[cfg(feature = "metadata")]
    #[test]
    fn search_normalization_preserves_character_boundaries() {
        assert!(matches_name("Alarm Setpoint Mode", "s e T pO iNt"));
        assert!(matches_name("Mİ X", "İx"));
        assert!(!matches_name("Ä", "ä"));
        assert!(matches_name("", "   "));
        assert!(!matches_name("counter", "counter value"));
        assert!(!matches_name("valve output", "valve\toutput"));
    }

    #[test]
    fn channel_addressing_leaves_global_parameters_shared() {
        assert_eq!(
            for_channel(by_dde(205).unwrap().parameter, 16)
                .unwrap()
                .process,
            48
        );
        let id = by_dde(1).unwrap().parameter;
        assert_eq!(for_channel(id, 2), Some(id));
        assert_eq!(for_channel(id, 0), None);
        assert_eq!(for_channel(id, 17), None);
    }
}
