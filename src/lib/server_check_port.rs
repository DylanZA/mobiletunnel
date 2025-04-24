/*
This file is part of MobileTunnel.
MobileTunnel is free software: you can redistribute it and/or modify it
under the terms of the GNU General Public License as published by the
Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

MobileTunnel is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY; without even the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.

 See the GNU General Public License for more details.
You should have received a copy of the GNU General Public License
along with MobileTunnel. If not, see <https://www.gnu.org/licenses/>.

Copyright 2024 Dylan Yudaken
*/

use std::io::{self, Write};
use std::path::Path;
use sysinfo::{Process, System};

fn abs_path(p: &Path) -> Option<&str> {
    p.file_name().map(|x| x.to_str()).flatten()
}

fn process_matches(this_exe: &str, process: &Process) -> bool {
    let this_exe_path = abs_path(Path::new(this_exe));
    let process_file_name = &process.exe().and_then(|p| abs_path(p));
    this_exe_path
        .and_then(|this| process_file_name.map(|that| this == that))
        .unwrap_or(false)
}

fn check_server_port(this_exe: &str, check_pid: u32) -> Result<bool, Box<dyn std::error::Error>> {
    let sys = System::new_all();
    for (pid, process) in sys.processes() {
        if pid.as_u32() == check_pid {
            if !process_matches(this_exe, process) {
                return Ok(false);
            }
            return Ok(true);
        }
    }
    return Ok(false);
}

pub fn run_check_server_port(
    this_exe: &str,
    check_pid: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut iter: u64 = 0;
    loop {
        iter += 1;
        let is_running = check_server_port(this_exe, check_pid)?;
        if is_running {
            println!("GOOD {}", iter);
        } else {
            println!("BAD {}", iter);
        }
        io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
