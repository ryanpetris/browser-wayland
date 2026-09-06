use schemars::JsonSchema;
use serde::Deserialize;

/// One optional screenshot size. Dimensions are image pixels, not logical desktop pixels.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotSizing {
    /// Output width in whole pixels, 1..=16384 and at most twice the native width. Height follows the source aspect ratio.
    pub width: Option<f64>,
    /// Output height in whole pixels, 1..=16384 and at most twice the native height. Width follows the source aspect ratio.
    pub height: Option<f64>,
    /// Percentage of native dimensions, greater than zero and at most 200.
    pub percentage: Option<f64>,
}

impl SnapshotSizing {
    pub fn validate(self) -> Result<(), &'static str> {
        if [self.width, self.height, self.percentage].iter().flatten().count() > 1 {
            return Err("supply only one of width, height, or percentage");
        }
        for value in [self.width, self.height].into_iter().flatten() {
            if !value.is_finite() || value < 1.0 || value > 16384.0 || value.fract() != 0.0 {
                return Err("width and height must be whole pixels from 1 to 16384");
            }
        }
        if self.percentage.is_some_and(|v| !v.is_finite() || v <= 0.0 || v > 200.0) {
            return Err("percentage must be greater than 0 and at most 200");
        }
        Ok(())
    }

    /// Round to nearest pixel, ties up, with a one-pixel minimum on each axis.
    /// Resolve against live geometry before allocating or rendering a capture.
    pub fn resolve(self, native_width: f64, native_height: f64) -> Result<(i32, i32, f64), &'static str> {
        self.validate()?;
        if !native_width.is_finite() || !native_height.is_finite() || native_width <= 0.0 || native_height <= 0.0 {
            return Err("capture target has no positive native dimensions");
        }
        let ratio = if let Some(width) = self.width { width / native_width }
            else if let Some(height) = self.height { height / native_height }
            else { self.percentage.map(|p| p / 100.0).unwrap_or(1.0) };
        if !ratio.is_finite() || ratio <= 0.0 { return Err("sizing ratio cannot be represented"); }
        if ratio > 2.0 { return Err("capture may not exceed twice the native dimensions"); }
        let width = (native_width * ratio).round().max(1.0);
        let height = (native_height * ratio).round().max(1.0);
        if width > 16384.0 || height > 16384.0 || width * height > (64 << 20) as f64 {
            return Err("capture exceeds 16384 pixels per side or 67108864 pixels in total");
        }
        Ok((width as i32, height as i32, ratio))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizing_and_limits() {
        for (w, h) in [(1920.0, 1080.0), (1080.0, 1920.0), (1501.5, 901.5)] {
            assert_eq!(SnapshotSizing::default().resolve(w, h).unwrap(), (w.round() as i32, h.round() as i32, 1.0));
            let width = SnapshotSizing { width: Some(64.0), ..Default::default() };
            assert_eq!(width.resolve(w, h).unwrap(), (64, (h * 64.0 / w).round().max(1.0) as i32, 64.0 / w));
            let height = SnapshotSizing { height: Some(40.0), ..Default::default() };
            assert_eq!(height.resolve(w, h).unwrap().0, (w * 40.0 / h).round().max(1.0) as i32);
        }
        assert_eq!(SnapshotSizing { width: Some(64.0), ..Default::default() }.resolve(640.0, 360.0).unwrap(), (64, 36, 0.1));
        assert_eq!(SnapshotSizing { height: Some(40.0), ..Default::default() }.resolve(300.0, 600.0).unwrap().0, 20);
        assert_eq!(SnapshotSizing { percentage: Some(50.0), ..Default::default() }.resolve(3.0, 5.0).unwrap(), (2, 3, 0.5));
        assert!(SnapshotSizing { width: Some(100.0), ..Default::default() }.resolve(3.0, 2.0).is_err());
        assert_eq!(SnapshotSizing { percentage: Some(0.001), ..Default::default() }.resolve(3.0, 2.0).unwrap().0, 1);
        for value in [0.0, -1.0, f64::NAN, f64::INFINITY, 201.0] {
            assert!(SnapshotSizing { percentage: Some(value), ..Default::default() }.validate().is_err());
        }
        assert!(SnapshotSizing { width: Some(1.5), ..Default::default() }.validate().is_err());
        assert!(SnapshotSizing { width: Some(64.0), percentage: Some(100.0), ..Default::default() }.validate().is_err());
        assert!(SnapshotSizing::default().resolve(16384.0, 16384.0).is_err());
        assert!(SnapshotSizing { percentage: Some(f64::from_bits(1)), ..Default::default() }.resolve(1920.0, 1080.0).is_err());
        assert!(serde_json::from_str::<SnapshotSizing>(r#"{"width":1,"width":2}"#).is_err());
        assert!(serde_json::from_str::<SnapshotSizing>(r#"{"scale":0.5}"#).is_err());
    }
}
