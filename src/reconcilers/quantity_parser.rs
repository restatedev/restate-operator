use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use regex::Regex;
use std::{num::ParseIntError, sync::OnceLock};

/// Adapted from https://github.com/sombralibre/k8s-quantity-parser to resolve dependency conflict
/// MIT licensed, Copyright (c) 2022 Alejandro Llanes

#[allow(non_camel_case_types)]
enum QuantityMemoryUnits {
    Ki,
    Mi,
    Gi,
    Ti,
    Pi,
    Ei,
    k,
    M,
    G,
    T,
    P,
    E,
    m,
    Invalid,
}

impl QuantityMemoryUnits {
    fn new(unit: &str) -> Self {
        match unit {
            "Ki" => Self::Ki,
            "Mi" => Self::Mi,
            "Gi" => Self::Gi,
            "Ti" => Self::Ti,
            "Pi" => Self::Pi,
            "Ei" => Self::Ei,
            "k" => Self::k,
            "M" => Self::M,
            "G" => Self::G,
            "T" => Self::T,
            "P" => Self::P,
            "E" => Self::E,
            "m" => Self::m,
            _ => Self::Invalid,
        }
    }
}

/// This trait works as a parser for the values retrieved from BTreeMap<String, Quantity> collections
/// in `k8s_openapi::api::core::v1::Pod` and `k8s_openapi::api::core::v1::Node`
///
/// # Errors
/// The parser will fails if encounters an invalid unit letters or failed to parse String to i64

pub trait QuantityParser {
    /// This method will parse the memory resource values returned by Kubernetes Api
    ///
    /// ```rust
    /// # use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    /// # use k8s_quantity_parser::QuantityParser;
    /// #
    /// let mib = Quantity("1Mi".into());
    /// let ret: i64 = 1048576;
    /// assert_eq!(mib.to_bytes().ok().flatten().unwrap(), ret);
    /// ```
    ///
    /// # Errors
    ///
    /// The parser will fails if encounters an invalid unit letters or failed to parse String to i64
    ///
    fn to_bytes(&self) -> Result<Option<i64>, ParseError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    ParseIntError(#[from] ParseIntError),
    #[error("Invalid memory unit")]
    InvalidMemoryUnit,
}

impl QuantityParser for Quantity {
    fn to_bytes(&self) -> Result<Option<i64>, ParseError> {
        let unit_str = &self.0;
        static REGEX: OnceLock<Regex> = OnceLock::new();
        let cap = REGEX
            .get_or_init(|| Regex::new(r"([[:alpha:]]{1,2}$)").unwrap())
            .captures(unit_str);

        if cap.is_none() {
            return Ok(Some(unit_str.parse::<i64>()?));
        };

        // Is safe to use unwrap here, as the value is already checked.
        match cap.unwrap().get(0) {
            Some(m) => match QuantityMemoryUnits::new(m.as_str()) {
                QuantityMemoryUnits::Ki => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some(amount * 1024))
                }
                QuantityMemoryUnits::Mi => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some((amount * 1024) * 1024))
                }
                QuantityMemoryUnits::Gi => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some(((amount * 1024) * 1024) * 1024))
                }
                QuantityMemoryUnits::Ti => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some((((amount * 1024) * 1024) * 1024) * 1024))
                }
                QuantityMemoryUnits::Pi => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some(((((amount * 1024) * 1024) * 1024) * 1024) * 1024))
                }
                QuantityMemoryUnits::Ei => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some(
                        (((((amount * 1024) * 1024) * 1024) * 1024) * 1024) * 1024,
                    ))
                }
                QuantityMemoryUnits::k => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some(amount * 1000))
                }
                QuantityMemoryUnits::M => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some((amount * 1000) * 1000))
                }
                QuantityMemoryUnits::G => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some(((amount * 1000) * 1000) * 1000))
                }
                QuantityMemoryUnits::T => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some((((amount * 1000) * 1000) * 1000) * 1000))
                }
                QuantityMemoryUnits::P => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some(((((amount * 1000) * 1000) * 1000) * 1000) * 1000))
                }
                QuantityMemoryUnits::E => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some(
                        (((((amount * 1000) * 1000) * 1000) * 1000) * 1000) * 1000,
                    ))
                }
                QuantityMemoryUnits::m => {
                    let unit_str = unit_str.replace(m.as_str(), "");
                    let amount = unit_str.parse::<i64>()?;
                    Ok(Some(amount / 1000))
                }
                QuantityMemoryUnits::Invalid => Err(ParseError::InvalidMemoryUnit),
            },
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_bytes_works() {
        assert!(Quantity("12345".into()).to_bytes().is_ok())
    }

    #[test]
    fn to_bytes_is_some() {
        assert!(Quantity("12345".into()).to_bytes().unwrap().is_some())
    }

    #[test]
    fn invalid_unit_fails() {
        assert!(Quantity("12345r".into()).to_bytes().is_err())
    }

    #[test]
    fn parse_i64_fails() {
        assert!(Quantity("123.123".into()).to_bytes().is_err())
    }

    #[test]
    fn is_none_value() {
        assert!(Quantity("0Mi".into()).to_bytes().unwrap().is_some())
    }

    #[test]
    fn pow2_mb_to_bytes() {
        let mib = Quantity("1Mi".into());
        let ret: i64 = 1048576;
        assert_eq!(mib.to_bytes().ok().flatten().unwrap(), ret);
    }

    #[test]
    fn pow10_gb_to_bytes() {
        let mib = Quantity("1G".into());
        let ret: i64 = 1000000000;
        assert_eq!(mib.to_bytes().ok().flatten().unwrap(), ret);
    }
}
