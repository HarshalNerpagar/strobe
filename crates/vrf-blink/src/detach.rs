use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::Command;

/// A process started by [`spawn_detached`], which outlives the one that started it.
pub struct Detached {
    pid: u32,
    #[cfg(unix)]
    child: std::process::Child,
    #[cfg(windows)]
    process: windows::Win32::Foundation::HANDLE,
}

impl Detached {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Whether the process has exited.
    pub fn exited(&mut self) -> bool {
        #[cfg(unix)]
        {
            !matches!(self.child.try_wait(), Ok(None))
        }
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::WAIT_TIMEOUT;
            use windows::Win32::System::Threading::WaitForSingleObject;
            unsafe { WaitForSingleObject(self.process, 0) != WAIT_TIMEOUT }
        }
    }
}

#[cfg(windows)]
impl Drop for Detached {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.process) };
    }
}

fn open_log(log: &Path) -> io::Result<File> {
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir)?;
    }
    File::create(log)
}

/// Starts `command` as a process that outlives this one, with no console. Its input is empty
/// and its output and errors go to `log`, which is truncated. It holds no other handle of this
/// process: a caller that reads this process's output through a pipe sees the end of it when
/// this process exits, not when the detached one does.
pub fn spawn_detached(command: &mut Command, log: &Path) -> io::Result<Detached> {
    let log = open_log(log)?;
    #[cfg(unix)]
    {
        use std::process::Stdio;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        std::os::unix::process::CommandExt::process_group(command, 0);
        let child = command.spawn()?;
        Ok(Detached {
            pid: child.id(),
            child,
        })
    }
    #[cfg(windows)]
    {
        windows_spawn(command, &log)
    }
}

#[cfg(windows)]
fn windows_spawn(command: &Command, log: &File) -> io::Result<Detached> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };
    use windows::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, UpdateProcThreadAttribute,
    };
    use windows::core::{PCWSTR, PWSTR};

    let null = File::open("NUL")?;
    let out = log.try_clone()?;
    let handles = [
        HANDLE(null.as_raw_handle()),
        HANDLE(log.as_raw_handle()),
        HANDLE(out.as_raw_handle()),
    ];
    for handle in handles {
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
            .map_err(io::Error::other)?;
    }

    let mut size = 0usize;
    let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
    let mut buffer = vec![0u64; size.div_ceil(8)];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
    unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }
        .map_err(io::Error::other)?;
    let listed = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(handles.as_ptr().cast()),
            std::mem::size_of_val(&handles),
            None,
            None,
        )
    };
    if let Err(error) = listed {
        unsafe { DeleteProcThreadAttributeList(list) };
        return Err(io::Error::other(error));
    }

    let mut info = STARTUPINFOEXW::default();
    info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    info.StartupInfo.hStdInput = handles[0];
    info.StartupInfo.hStdOutput = handles[1];
    info.StartupInfo.hStdError = handles[2];
    info.lpAttributeList = list;

    let program = Path::new(command.get_program())
        .is_absolute()
        .then(|| wide(command.get_program()));
    let mut line = command_line(command);
    let mut environment = environment_block(command);
    let directory = command.get_current_dir().map(|dir| wide(dir.as_os_str()));
    let mut process = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            program
                .as_ref()
                .map_or(PCWSTR::null(), |program| PCWSTR(program.as_ptr())),
            Some(PWSTR(line.as_mut_ptr())),
            None,
            None,
            true,
            DETACHED_PROCESS
                | CREATE_NEW_PROCESS_GROUP
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            Some(environment.as_mut_ptr().cast()),
            directory
                .as_ref()
                .map_or(PCWSTR::null(), |dir| PCWSTR(dir.as_ptr())),
            &info.StartupInfo,
            &mut process,
        )
    };
    unsafe { DeleteProcThreadAttributeList(list) };
    created.map_err(io::Error::other)?;
    let _ = unsafe { CloseHandle(process.hThread) };
    Ok(Detached {
        pid: process.dwProcessId,
        process: process.hProcess,
    })
}

#[cfg(windows)]
fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().chain([0]).collect()
}

#[cfg(windows)]
fn command_line(command: &Command) -> Vec<u16> {
    let mut line = Vec::new();
    quote(command.get_program(), &mut line);
    for arg in command.get_args() {
        line.push(u16::from(b' '));
        quote(arg, &mut line);
    }
    line.push(0);
    line
}

#[cfg(windows)]
fn quote(arg: &std::ffi::OsStr, line: &mut Vec<u16>) {
    use std::os::windows::ffi::OsStrExt;
    let (space, tab, mark, slash) = (
        u16::from(b' '),
        u16::from(b'\t'),
        u16::from(b'"'),
        u16::from(b'\\'),
    );
    let units: Vec<u16> = arg.encode_wide().collect();
    if !units.is_empty() && !units.iter().any(|&unit| [space, tab, mark].contains(&unit)) {
        line.extend(units);
        return;
    }
    line.push(mark);
    let mut slashes = 0;
    for unit in units {
        if unit == slash {
            slashes += 1;
        } else {
            if unit == mark {
                line.extend(std::iter::repeat_n(slash, slashes + 1));
            }
            slashes = 0;
        }
        line.push(unit);
    }
    line.extend(std::iter::repeat_n(slash, slashes));
    line.push(mark);
}

#[cfg(windows)]
fn environment_block(command: &Command) -> Vec<u16> {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    let key = |name: &std::ffi::OsStr| name.to_string_lossy().to_uppercase();
    let mut vars: BTreeMap<String, (OsString, OsString)> = std::env::vars_os()
        .map(|(name, value)| (key(&name), (name, value)))
        .collect();
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => {
                vars.insert(key(name), (name.to_owned(), value.to_owned()));
            }
            None => {
                vars.remove(&key(name));
            }
        }
    }
    let mut block = Vec::new();
    for (name, value) in vars.values() {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::{Duration, Instant};

    fn line_of(program: &str, args: &[&str]) -> String {
        let mut command = Command::new(program);
        command.args(args);
        let line = command_line(&command);
        String::from_utf16(&line[..line.len() - 1]).unwrap()
    }

    #[test]
    fn a_command_line_quotes_arguments_the_way_the_c_runtime_splits_them() {
        assert_eq!(
            line_of(r"C:\a b\strobe.exe", &["daemon", "--log", r"C:\x y\d.log"]),
            r#""C:\a b\strobe.exe" daemon --log "C:\x y\d.log""#
        );
        assert_eq!(line_of("p", &[""]), r#"p """#);
        assert_eq!(line_of("p", &[r"a\b"]), r"p a\b");
        assert_eq!(line_of("p", &[r"a b\"]), r#"p "a b\\""#);
        assert_eq!(line_of("p", &[r#"say "hi""#]), r#"p "say \"hi\"""#);
        assert_eq!(line_of("p", &[r#"a\"b"#]), r#"p "a\\\"b""#);
    }

    #[test]
    fn a_detached_process_holds_no_inheritable_handle_of_its_parent() -> io::Result<()> {
        use windows::Win32::Foundation::{
            HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
        };
        let dir = std::env::temp_dir().join(format!("vrf-detach-{}", std::process::id()));
        let log = dir.join("child.log");
        let (mut reader, writer) = io::pipe()?;
        let raw = HANDLE(std::os::windows::io::AsRawHandle::as_raw_handle(&writer));
        unsafe { SetHandleInformation(raw, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
            .map_err(io::Error::other)?;
        let mut command = Command::new("cmd");
        command
            .args(["/c", "echo detached & ping -n 4 127.0.0.1 >nul"])
            .env("VRF_DETACH_TEST", "1");
        let mut child = spawn_detached(&mut command, &log)?;
        let _ = unsafe { SetHandleInformation(raw, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
        drop(writer);
        let start = Instant::now();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest)?;
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(!child.exited());
        let deadline = Instant::now() + Duration::from_secs(20);
        while !child.exited() {
            assert!(Instant::now() < deadline, "the detached child never exited");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(fs::read_to_string(&log)?.contains("detached"));
        fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
