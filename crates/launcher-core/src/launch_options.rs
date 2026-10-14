use std::ffi::{OsStr, OsString};

use thiserror::Error;

/// User preferences are sampled once when Play is handled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserLaunchOptions {
    pub max_memory_mib: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapMemory {
    pub min_memory_mib: u32,
    pub max_memory_mib: u32,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("invalid heap range: min={min_memory_mib}, max={max_memory_mib} MiB")]
pub struct InvalidHeapMemory {
    pub min_memory_mib: u32,
    pub max_memory_mib: u32,
}

impl HeapMemory {
    pub fn new(min_memory_mib: u32, max_memory_mib: u32) -> Result<Self, InvalidHeapMemory> {
        if min_memory_mib == 0 || max_memory_mib == 0 || min_memory_mib > max_memory_mib {
            return Err(InvalidHeapMemory {
                min_memory_mib,
                max_memory_mib,
            });
        }
        Ok(Self {
            min_memory_mib,
            max_memory_mib,
        })
    }

    pub fn resolve(
        backend_min: u32,
        backend_max: u32,
        options: UserLaunchOptions,
    ) -> Result<Self, InvalidHeapMemory> {
        // Validate the original backend values before applying the user's override.
        Self::new(backend_min, backend_max)?;
        let maximum = options.max_memory_mib.unwrap_or(backend_max);
        Self::new(backend_min.min(maximum), maximum)
    }

    pub(crate) fn replace_arguments(
        self,
        arguments: &mut Vec<OsString>,
    ) -> Result<(), InvalidHeapMemory> {
        Self::new(self.min_memory_mib, self.max_memory_mib)?;
        arguments.retain(|argument| !is_heap_argument(argument));
        arguments.insert(0, format!("-Xmx{}M", self.max_memory_mib).into());
        arguments.insert(0, format!("-Xms{}M", self.min_memory_mib).into());
        Ok(())
    }
}

pub(crate) fn is_heap_argument(argument: &OsStr) -> bool {
    let argument = argument.to_string_lossy();
    argument.starts_with("-Xms")
        || argument.starts_with("-Xmx")
        || argument.starts_with("-XX:InitialHeapSize=")
        || argument.starts_with("-XX:MaxHeapSize=")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_backend_before_overriding_and_caps_initial_heap() {
        let options = UserLaunchOptions {
            max_memory_mib: Some(1536),
        };
        assert_eq!(
            HeapMemory::resolve(2048, 4096, options).unwrap(),
            HeapMemory {
                min_memory_mib: 1536,
                max_memory_mib: 1536,
            }
        );
        assert!(HeapMemory::resolve(4096, 2048, options).is_err());
        assert!(HeapMemory::resolve(0, 4096, options).is_err());
        assert!(
            HeapMemory::resolve(
                1024,
                4096,
                UserLaunchOptions {
                    max_memory_mib: Some(0)
                }
            )
            .is_err()
        );
        assert_eq!(
            HeapMemory::resolve(1024, 4096, UserLaunchOptions::default())
                .unwrap()
                .max_memory_mib,
            4096
        );
    }

    #[test]
    fn replaces_metadata_heap_aliases_and_preserves_other_arguments() {
        let mut args = [
            "-Xmx2G",
            "-Xms512M",
            "-XX:MaxHeapSize=123",
            "-XX:InitialHeapSize=45",
            "-Dfile.encoding=UTF-8",
            "-cp",
            "libraries",
        ]
        .map(OsString::from)
        .to_vec();
        HeapMemory::new(2048, 6144)
            .unwrap()
            .replace_arguments(&mut args)
            .unwrap();
        assert_eq!(
            args,
            [
                "-Xms2048M",
                "-Xmx6144M",
                "-Dfile.encoding=UTF-8",
                "-cp",
                "libraries"
            ]
            .map(OsString::from)
        );
    }
}
