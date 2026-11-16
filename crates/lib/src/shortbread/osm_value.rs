//! Parsing policies for numeric OpenStreetMap tag values.

/// Parse a length-like OSM value into meters.
///
/// A single comma followed by exactly three digits is treated as a thousands
/// separator. Other single commas are decimal separators, and repeated commas
/// are treated as grouping separators. A dot-decimal value treats commas as
/// grouping separators. The first semicolon-delimited value is used. Bare
/// values and values with meter units are meters; feet units and the common
/// feet-and-inches form are converted to meters.
pub(crate) fn parse_length_m(raw: &str) -> Option<f64> {
    let mut value = raw.trim().to_ascii_lowercase();
    if let Some((first, _)) = value.split_once(';') {
        value = first.trim().to_string();
    }
    if value.is_empty() {
        return None;
    }

    if let Some(meters) = parse_feet_inches(&value) {
        return meters.is_finite().then_some(meters);
    }

    let mut feet = false;
    if value.ends_with("feet") {
        feet = true;
        value.truncate(value.len().saturating_sub(4));
    } else if value.ends_with("ft") {
        feet = true;
        value.truncate(value.len().saturating_sub(2));
    } else if value.ends_with('\'') {
        feet = true;
        value.truncate(value.len().saturating_sub(1));
    } else if value.ends_with("meters") {
        value.truncate(value.len().saturating_sub(6));
    } else if value.ends_with("meter") {
        value.truncate(value.len().saturating_sub(5));
    } else if value.ends_with('m') {
        value.truncate(value.len().saturating_sub(1));
    }

    let token = value.split_whitespace().next()?;
    let number = parse_decimal(token)?;
    let meters = if feet { number * 0.3048 } else { number };
    meters.is_finite().then_some(meters)
}

/// Parse a nonnegative count, retaining fractional values such as building
/// levels. Comma and semicolon handling follows the policies documented by
/// `parse_length_m`.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn parse_count(raw: &str) -> Option<f64> {
    let token = raw.split(';').next()?.trim();
    if token.is_empty() {
        return None;
    }

    if !token.contains('.')
        && !token.contains(',')
        && let Some(count) = parse_int(token)
    {
        return (count >= 0).then_some(count as f64);
    }

    let count = parse_decimal(token)?;
    (count >= 0.0).then_some(count)
}

/// Parse an integer using the legacy integer-tag locale policy: commas are
/// always removed as grouping separators, so `"1,5"` means 15. Decimal
/// separators are not accepted. Whitespace is not stripped here.
pub(crate) fn parse_int(raw: &str) -> Option<i64> {
    raw.parse::<i64>()
        .ok()
        .or_else(|| raw.replace(',', "").parse::<i64>().ok())
}

fn parse_feet_inches(value: &str) -> Option<f64> {
    let (feet_token, inches_with_quote) = value.split_once('\'')?;
    let inches_token = inches_with_quote.strip_suffix('"')?.trim();
    let feet = parse_decimal(feet_token)?;
    let inches = parse_decimal(inches_token)?;
    let magnitude = feet.abs() * 0.3048 + inches.abs() * 0.0254;
    let negative = feet_token.trim_start().starts_with('-');
    Some(if negative { -magnitude } else { magnitude })
}

fn parse_decimal(token: &str) -> Option<f64> {
    let normalized = normalize_decimal_token(token);
    let number = normalized.parse::<f64>().ok()?;
    number.is_finite().then_some(number)
}

fn normalize_decimal_token(token: &str) -> String {
    let token = token.trim();
    if !token.contains(',') {
        return token.to_string();
    }
    if token.contains('.') {
        return token.replace(',', "");
    }

    let comma_count = token.bytes().filter(|&byte| byte == b',').count();
    if comma_count == 1
        && let Some((left, right)) = token.split_once(',')
    {
        if right.len() == 3 && !left.is_empty() {
            return [left, right].concat();
        }
        return [left, ".", right].concat();
    }

    token.replace(',', "")
}
