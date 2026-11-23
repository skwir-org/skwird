//! Pipes on Windows as users other than the test's own see them
//! (`docs/DESIGN.md`, Protocol; `docs/PLAN.md`, phase 4, step 7): the
//! system's pipe, which every user may reach and none may answer, and a
//! pipe another user made, which a client refuses.
//!
//! In a test binary of its own: each test makes a user and loads their
//! profile, which loads the machine for seconds, and cargo runs one test
//! binary at a time, so that no test that times what the engine does
//! shares the machine with them (`tests/windows.rs`: a sync that checks
//! once took past its bound beside them). On a machine of the tests' own
//! alone (`skwird_testkit::throwaway`).

#![cfg(windows)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use skwird_testkit::Scratch;

/// The system's pipe (`skwird_client::pipe::Server::bind_system`): its
/// owner, as an administrator makes it, is the administrators, and a
/// client takes it so; a user who is not one may connect to it, but not
/// make an instance of it, which would answer the service's clients; an
/// administrator may. On a machine of the tests' own alone, which may make
/// a user.
#[test]
#[allow(unsafe_code)] // A pipe's instance made as another user has no safe binding; each block says why it is sound.
fn the_systems_pipe_is_every_users_to_reach_and_none_to_answer() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE};
    if !skwird_testkit::throwaway() {
        eprintln!("skipped: makes a user (SKWIRD_TEST_MACHINE)");
        return;
    }
    let name = format!(r"\\.\pipe\skwird-test-system-{}", std::process::id());
    let server = skwird_client::pipe::Server::bind_system(name.as_ref()).unwrap();
    let accepting = std::thread::spawn(move || {
        let stream = server.accept(Some(Duration::from_secs(10))).unwrap();
        (server, stream)
    });
    let client = skwird_client::pipe::Stream::connect(name.as_ref(), Some(Duration::from_secs(5)));
    assert!(client.is_ok(), "taken as the system's: {:?}", client.err());
    let (_server, accepted) = accepting.join().unwrap();
    assert!(accepted.is_some());
    // An instance made, as `ConnectNamedPipe`'s server makes one.
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let instance = || {
        // SAFETY: a NUL-terminated name; no security attributes: the
        // pipe's own descriptor decides.
        let pipe = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE,
                255,
                512,
                512,
                0,
                std::ptr::null(),
            )
        };
        let error = std::io::Error::last_os_error();
        if pipe != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            // SAFETY: the instance made, ours, closed once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(pipe) };
            return Ok(());
        }
        Err(error)
    };
    assert!(instance().is_ok(), "an administrator makes an instance");
    let user = skwird_testkit::User::make("skwird-pipe", "Sk-1wird.pipe!");
    let token = user.logon();
    let denied = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                // SAFETY: a primary token of ours, the thread's alone while
                // it makes its instance, then the thread's own again.
                assert_ne!(unsafe { ImpersonateLoggedOnUser(token.as_raw_handle()) }, 0);
                let made = instance();
                // SAFETY: no argument.
                assert_ne!(unsafe { RevertToSelf() }, 0);
                made
            })
            .join()
            .unwrap()
    });
    assert_eq!(
        denied.map_err(|e| e.raw_os_error()),
        Err(Some(5)),
        "a user makes no instance of it"
    );
}

/// A pipe another user made is refused, whatever its name and however
/// open it is: a user of the tests' own serves a pipe every user may read
/// and write, as one squatting the service's name would, and this user's
/// client refuses it before saying anything, its owner and its server's
/// user neither the system's nor its own. On a machine of the tests' own
/// alone.
#[test]
fn a_pipe_another_user_made_is_refused() {
    if !skwird_testkit::throwaway() {
        eprintln!("skipped: makes a user (SKWIRD_TEST_MACHINE)");
        return;
    }
    let s = Scratch::new("win-squat");
    let user = skwird_testkit::User::make("skwird-squat", "Sk-1wird.sq!");
    let folder = s.dir("squatter");
    skwird_testkit::grant(&folder, user.name);
    let name = format!("skwird-test-squat-{}", std::process::id());
    let script = s.write(
        r"squatter\squat.ps1",
        format!(
            "$security = New-Object System.IO.Pipes.PipeSecurity\n\
             $everyone = New-Object System.Security.Principal.SecurityIdentifier('S-1-1-0')\n\
             $security.AddAccessRule((New-Object System.IO.Pipes.PipeAccessRule($everyone, 'ReadWrite', 'Allow')))\n\
             $pipe = New-Object System.IO.Pipes.NamedPipeServerStream('{name}', 'InOut', 1, 'Byte', 'None', 4096, 4096, $security)\n\
             $pipe.WaitForConnection()\n\
             Start-Sleep -Seconds 30\n"
        )
        .as_bytes(),
    );
    let system =
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let _squatter = user.spawn(&format!(
        "\"{}\" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
        system
            .join(r"System32\WindowsPowerShell\v1.0\powershell.exe")
            .display(),
        script.display()
    ));
    let pipe = format!(r"\\.\pipe\{name}");
    let deadline = Instant::now() + Duration::from_secs(60);
    let refused = loop {
        match skwird_client::pipe::Stream::connect(pipe.as_ref(), Some(Duration::from_secs(1))) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            said => break said,
        }
    };
    let Err(e) = refused else {
        panic!("another user's pipe was taken");
    };
    // Refused by the client's check, not by the pipe's own access.
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");
    assert!(e.raw_os_error().is_none(), "{e}");
    assert!(e.to_string().contains("another user's"), "{e}");
}
