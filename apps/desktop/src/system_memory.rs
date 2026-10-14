use std::io;

pub const MIN_MEMORY_MIB: u32 = 1024;
pub const MEMORY_STEP_MIB: u32 = 256;

#[derive(Clone, Copy, Debug)]
pub struct SystemMemory {
    pub total_mib: u64,
    pub available_mib: u64,
}

#[cfg(windows)]
pub fn query_system_memory() -> io::Result<SystemMemory> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX consists of integer fields. dwLength is initialized
    // to the required structure size before Windows writes to the valid pointer.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SystemMemory {
        total_mib: status.ullTotalPhys / (1024 * 1024),
        available_mib: status.ullAvailPhys / (1024 * 1024),
    })
}

#[cfg(not(windows))]
pub fn query_system_memory() -> io::Result<SystemMemory> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "physical memory detection requires Windows",
    ))
}

#[derive(Clone, Copy, Debug)]
pub struct MemoryBudget {
    pub maximum_mib: u32,
}

impl MemoryBudget {
    pub fn for_system(memory: SystemMemory) -> Result<Self, String> {
        // Round the 20% reserve upwards so it is never smaller than requested.
        let reserve = 2048.max(memory.total_mib.div_ceil(5));
        let maximum = memory.total_mib.saturating_sub(reserve) / u64::from(MEMORY_STEP_MIB)
            * u64::from(MEMORY_STEP_MIB);
        if maximum < u64::from(MIN_MEMORY_MIB) {
            return Err(
                "Недостаточно ОЗУ: после резерва для системы доступно меньше 1024 МБ".to_owned(),
            );
        }
        // Slint represents the range in a signed integer.
        let maximum_mib = maximum.min(i32::MAX as u64 / 256 * 256) as u32;
        Ok(Self { maximum_mib })
    }

    pub fn validate(self, value: u32) -> Result<u32, String> {
        if !(MIN_MEMORY_MIB..=self.maximum_mib).contains(&value) {
            return Err(format!(
                "Введите от {MIN_MEMORY_MIB} до {} МБ",
                self.maximum_mib
            ));
        }
        Ok(value)
    }

    pub fn parse(self, input: &str) -> Result<u32, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("Введите объём памяти в МБ".to_owned());
        }
        if !input.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("Введите целое положительное число в МБ".to_owned());
        }
        let value = input
            .parse::<u32>()
            .map_err(|_| "Число слишком большое".to_owned())?;
        self.validate(value)
    }

    pub fn suggested(self, value: u32) -> u32 {
        value.clamp(MIN_MEMORY_MIB, self.maximum_mib)
    }

    pub fn recommendation(self) -> String {
        let upper = self.maximum_mib.min(8192);
        let lower = upper.min(4096);
        if lower == upper {
            format!("Рекомендуется {lower} МБ для этого компьютера")
        } else {
            format!("Рекомендуется {lower}–{upper} МБ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_memory_and_does_not_depend_on_current_load() {
        for (total, maximum) in [(4096, 2048), (8192, 6144), (16384, 13056), (32768, 26112)] {
            for available in [100, total] {
                let budget = MemoryBudget::for_system(SystemMemory {
                    total_mib: total,
                    available_mib: available,
                })
                .unwrap();
                assert_eq!(budget.maximum_mib, maximum);
                assert!(!budget.recommendation().contains("8192") || maximum >= 8192);
            }
        }
        assert!(
            MemoryBudget::for_system(SystemMemory {
                total_mib: 2048,
                available_mib: 2048
            })
            .is_err()
        );
    }

    #[test]
    fn accepts_exact_integers_and_rejects_invalid_input() {
        let budget = MemoryBudget { maximum_mib: 6144 };
        assert_eq!(budget.parse("6143").unwrap(), 6143);
        assert_eq!(budget.parse(" 1024 ").unwrap(), 1024);
        for input in [
            "",
            " ",
            "-1024",
            "+2048",
            "1.5",
            "2e3",
            "abc",
            "1023",
            "6145",
            "4294967296",
        ] {
            assert!(budget.parse(input).is_err(), "{input}");
        }
        assert_eq!(budget.suggested(8192), 6144);
    }
}
