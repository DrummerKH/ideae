//! The battery, as far as the system tells: how full it is and whether it
//! is being used up. For one thing: to say once, when it runs low, that
//! there is a power save mode.

/// How full the battery is, 0 to 100, and whether the machine runs on it.
/// None where there is no battery. Reads a file or
/// runs a program: not for the UI thread.
pub fn read() -> Option<(u8, bool)> {
    #[cfg(target_os = "linux")]
    {
        let supplies = std::fs::read_dir("/sys/class/power_supply").ok()?;
        for supply in supplies.flatten() {
            let dir = supply.path();
            if std::fs::read_to_string(dir.join("type")).is_ok_and(|kind| kind.trim() == "Battery") {
                let full = std::fs::read_to_string(dir.join("capacity")).ok()?.trim().parse::<u8>().ok()?;
                let used = std::fs::read_to_string(dir.join("status")).is_ok_and(|status| status.trim() == "Discharging");
                return Some((full.min(100), used));
            }
        }
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let out = crate::procs::command("pmset").args(["-g", "batt"]).output().ok()?;
        return parse_pmset(&String::from_utf8_lossy(&out.stdout));
    }
    #[cfg(windows)]
    {
        // What the system keeps of its power: on the mains or not, and how full, 255 for not known.
        #[repr(C)]
        struct Power {
            mains: u8,
            flag: u8,
            full: u8,
            status: u8,
            left: u32,
            left_of_full: u32,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetSystemPowerStatus(power: *mut Power) -> i32;
        }
        let mut power = Power { mains: 255, flag: 255, full: 255, status: 0, left: 0, left_of_full: 0 };
        // SAFETY: the system fills the struct it is given; it has the layout of SYSTEM_POWER_STATUS.
        if unsafe { GetSystemPowerStatus(&mut power) } == 0 {
            return None;
        }
        return windows_power(power.mains, power.flag, power.full);
    }
    #[allow(unreachable_code)]
    None
}

/// Windows' own figures: the mains 0 for off and 1 for on, the flag's bit
/// 128 for no battery, the charge in hundredths or 255 for not known.
#[cfg(any(windows, test))]
fn windows_power(mains: u8, flag: u8, full: u8) -> Option<(u8, bool)> {
    (flag & 128 == 0 && flag != 255 && full <= 100).then_some((full, mains == 0))
}

/// `pmset -g batt`: " -InternalBattery-0 (id=…)\t47%; discharging; 3:10 remaining present: true".
#[cfg(any(target_os = "macos", test))]
fn parse_pmset(out: &str) -> Option<(u8, bool)> {
    let line = out.lines().find(|line| line.contains('%'))?;
    let before = line.split('%').next()?;
    let full: u8 = before.rsplit(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    // "discharging", not "charging" or "charged" or "AC attached; not charging".
    let used = line.split(';').any(|part| part.trim() == "discharging");
    Some((full.min(100), used))
}

/// Low enough to say so, and when it has been said, high enough to say it
/// again the next time it runs down.
pub const LOW: u8 = 30;
pub const AGAIN: u8 = 40;

/// Whether to tell now, and whether it has been told, after a reading:
/// once as the battery runs down past `LOW` while in use, and again only
/// after it was charged or back above `AGAIN`.
pub fn tell(read: Option<(u8, bool)>, told: bool) -> (bool, bool) {
    match read {
        Some((full, true)) if full <= LOW => (!told, true),
        Some((full, used)) if !used || full >= AGAIN => (false, false),
        _ => (false, told),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_pmset, tell, windows_power};

    #[test]
    fn the_battery_is_read_and_told_of_once_as_it_runs_down() {
        let on_battery = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1234)\t47%; discharging; 3:10 remaining present: true\n";
        assert_eq!(parse_pmset(on_battery), Some((47, true)));
        assert_eq!(parse_pmset("Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t100%; charged; 0:00 remaining present: true\n"), Some((100, false)));
        assert_eq!(parse_pmset(" -InternalBattery-0 (id=1)\t28%; AC attached; not charging present: true\n"), Some((28, false)));
        assert_eq!(parse_pmset("Now drawing from 'AC Power'\n"), None, "a machine with no battery");

        assert_eq!(windows_power(0, 0, 28), Some((28, true)), "off the mains");
        assert_eq!(windows_power(1, 8, 61), Some((61, false)), "charging");
        assert_eq!((windows_power(1, 128, 255), windows_power(255, 255, 255)), (None, None), "a desktop, and not known");

        // Down past the mark: told once, not at every look after it.
        assert_eq!(tell(Some((31, true)), false), (false, false));
        assert_eq!(tell(Some((30, true)), false), (true, true));
        assert_eq!(tell(Some((22, true)), true), (false, true));
        // Plugged in at 25%, and unplugged again still low: that is another running down.
        assert_eq!(tell(Some((25, false)), true), (false, false));
        assert_eq!(tell(Some((25, true)), false), (true, true));
        // A little above the mark and in use: neither told nor forgotten.
        assert_eq!(tell(Some((35, true)), true), (false, true));
        assert_eq!(tell(None, true), (false, true));
    }
}
