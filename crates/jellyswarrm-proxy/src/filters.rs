//! Custom Askama template filters

use askama::Values;

/// Round a floating point number to the specified decimal places
pub fn round(value: &f64, _values: &dyn Values, decimals: i32) -> ::askama::Result<f64> {
    let multiplier = 10f64.powi(decimals);
    Ok((value * multiplier).round() / multiplier)
}
