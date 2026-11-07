//! Relays MCP traffic between Claude Code (stdio) and a Bishop window (unix socket).

use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use gtk::glib;

pub fn run(socket: &str) -> glib::ExitCode {
    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("bishop: cannot connect to {socket}: {e}");
            return glib::ExitCode::FAILURE;
        }
    };
    let (Ok(mut reader), Ok(mut writer)) = (stream.try_clone(), stream.try_clone()) else {
        eprintln!("bishop: cannot clone socket handle");
        return glib::ExitCode::FAILURE;
    };

    let to_stdout = std::thread::spawn(move || {
        let _ = io::copy(&mut reader, &mut io::stdout().lock());
    });
    let _ = io::copy(&mut io::stdin().lock(), &mut writer);
    let _ = stream.shutdown(Shutdown::Write);
    let _ = to_stdout.join();
    glib::ExitCode::SUCCESS
}
