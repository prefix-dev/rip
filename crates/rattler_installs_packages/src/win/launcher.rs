//! This module contains the code to create a launcher executable for windows.

use std::{
    env,
    io::{Cursor, Write},
};
use zip::{write::FileOptions, ZipWriter};

/// Defines the type of script to run. This is either a GUI application or a console application.
/// When running a console application a terminal is expected. When running a GUI application the
/// user should not see a terminal.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LauncherType {
    /// A GUI application, the application will not spawn a terminal/console.
    Gui,

    /// A console application, the application will be run inside a terminal/console.
    Console,
}

/// Defines the architecture of the launcher executable that is created for every entry point on
/// windows.
#[derive(Copy, Clone, Hash, Eq, PartialEq, Debug)]
pub enum WindowsLauncherArch {
    /// Create a launcher for the x86 architecture.
    X86,

    /// Create a launcher for the x86_64 architecture.
    X86_64,

    /// Create a launcher for the arm64 architecture.
    Arm64,
}

impl WindowsLauncherArch {
    /// Try determine the current architecture from the environment. Returns `None` if the
    /// architecture could not be determined or is not supported.
    pub fn current() -> Option<Self> {
        match env::consts::ARCH {
            "x86" => Some(Self::X86),
            "x86_64" => Some(Self::X86_64),
            "aarch64" => Some(Self::Arm64),
            _ => None,
        }
    }

    /// Returns the bytes of the launcher executable for this architecture.
    pub fn launcher_bytes(self, script_type: LauncherType) -> &'static [u8] {
        match (self, script_type) {
            (Self::X86, LauncherType::Console) => include_bytes!("./windows-launcher/t32.exe"),
            (Self::X86_64, LauncherType::Console) => include_bytes!("./windows-launcher/t64.exe"),
            (Self::Arm64, LauncherType::Console) => {
                include_bytes!("./windows-launcher/t64-arm.exe")
            }
            (Self::X86, LauncherType::Gui) => include_bytes!("./windows-launcher/w32.exe"),
            (Self::X86_64, LauncherType::Gui) => include_bytes!("./windows-launcher/w64.exe"),
            (Self::Arm64, LauncherType::Gui) => include_bytes!("./windows-launcher/w64-arm.exe"),
        }
    }
}

/// Constructs an executable that can be used to launch a python script on Windows.
pub fn build_windows_launcher(
    shebang: &str,
    launcher_python_script: &[u8],
    launcher_arch: WindowsLauncherArch,
    script_type: LauncherType,
) -> Vec<u8> {
    let mut launcher = launcher_arch.launcher_bytes(script_type).to_vec();

    // We'r e using the zip writer,but it turns out we're not actually deflating apparently
    // we're just using an offset
    // https://github.com/pypa/distlib/blob/8ed03aab48add854f377ce392efffb79bb4d6091/PC/launcher.c#L259-L271
    let mut stream: Vec<u8> = Vec::new();
    {
        let stored = FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let mut archive = ZipWriter::new(Cursor::new(&mut stream));
        let error_msg = "Writing to Vec<u8> should never fail";
        archive.start_file("__main__.py", stored).expect(error_msg);
        archive.write_all(launcher_python_script).expect(error_msg);
        archive.finish().expect(error_msg);
    }

    launcher.append(&mut format!("{}\n", shebang.trim()).into_bytes());
    launcher.append(&mut stream);
    launcher
}
