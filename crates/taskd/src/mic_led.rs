//! The Caps Lock LED as a recording light.
//!
//! Push-to-talk is bound to Caps Lock (which the AgentOS session stops from
//! toggling caps), so its keyboard LED is free: it is lit exactly while the
//! microphone records. The kernel exposes keyboard LEDs as
//! `/sys/class/leds/inputN::capslock/brightness`; the image's udev rule makes
//! them writable by the `input` group, which the desktop user is in.
//!
//! Best effort: a machine without such an LED, or without permission, simply
//! has no light (the on-screen indicator still shows the state).

use std::path::PathBuf;

pub struct MicLed {
    dir: Option<PathBuf>,
}

impl MicLed {
    /// `AGENTOS_MIC_LED=off` disables the light; any other value is the LED
    /// class directory to use instead of `/sys/class/leds` (for tests).
    pub fn from_env() -> Self {
        match std::env::var("AGENTOS_MIC_LED") {
            Ok(v) if v == "off" => Self { dir: None },
            Ok(v) if !v.is_empty() => Self::new(v),
            _ => Self::new("/sys/class/leds"),
        }
    }

    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
        }
    }

    pub fn disabled() -> Self {
        Self { dir: None }
    }

    /// Light (or clear) every keyboard's Caps Lock LED.
    pub fn set(&self, on: bool) {
        let Some(dir) = &self.dir else { return };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with("::capslock") {
                let _ = std::fs::write(e.path().join("brightness"), if on { "1" } else { "0" });
            }
        }
    }
}

impl Drop for MicLed {
    /// Never leave the light on when the daemon goes away mid-recording.
    fn drop(&mut self) {
        self.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lights_only_capslock_leds() {
        let dir = std::env::temp_dir().join(format!("agentos-led-{}", std::process::id()));
        for name in ["input3::capslock", "input7::capslock", "input3::numlock"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("brightness"), "0").unwrap();
        }
        let read = |n: &str| std::fs::read_to_string(dir.join(n).join("brightness")).unwrap();

        let led = MicLed::new(&dir);
        led.set(true);
        assert_eq!(read("input3::capslock"), "1");
        assert_eq!(read("input7::capslock"), "1");
        assert_eq!(read("input3::numlock"), "0");
        drop(led);
        assert_eq!(read("input3::capslock"), "0");
        assert_eq!(read("input7::capslock"), "0");

        // Missing directory or disabled: nothing happens, nothing panics.
        MicLed::new(dir.join("nope")).set(true);
        MicLed::disabled().set(true);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
