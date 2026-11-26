//! Checks the user's own login password, to turn on Danger Mode.
//!
//! Uses PAM (through the `login` service, as screen lockers do), loaded at run time so Bishop
//! builds without PAM's development files. Running as the user, PAM's `pam_unix` checks the
//! password with its setuid helper, which only ever checks the caller's own account.
//!
//! Once the password has been given, Danger Mode can be turned on again without it until
//! Bishop quits. That is kept in memory only: nothing on disk, which any program running as
//! the user (the agent's commands included) could write, can unlock it.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static UNLOCKED: AtomicBool = AtomicBool::new(false);

/// Whether the password has been given since Bishop started.
pub fn danger_unlocked() -> bool {
    UNLOCKED.load(Ordering::SeqCst)
}

pub fn unlock_danger() {
    UNLOCKED.store(true, Ordering::SeqCst);
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Correct,
    Wrong,
    /// The password couldn't be checked, and why.
    Unavailable(String),
}

const SERVICE: &CStr = c"login";

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_AUTH_ERR: c_int = 7;
const PAM_CONV_ERR: c_int = 19;
const PAM_MAXTRIES: c_int = 11;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;
const PAM_DISALLOW_NULL_AUTHTOK: c_int = 0x0001;

#[repr(C)]
struct Message {
    style: c_int,
    text: *const c_char,
}

#[repr(C)]
struct Response {
    resp: *mut c_char,
    retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(c_int, *mut *const Message, *mut *mut Response, *mut c_void) -> c_int;

#[repr(C)]
struct Conv {
    conv: ConvFn,
    data: *mut c_void,
}

type Start = unsafe extern "C" fn(*const c_char, *const c_char, *const Conv, *mut *mut c_void) -> c_int;
type Authenticate = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type End = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type StrError = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;

struct Pam {
    start: Start,
    authenticate: Authenticate,
    end: End,
    strerror: StrError,
}

fn pam() -> Option<&'static Pam> {
    static PAM: OnceLock<Option<Pam>> = OnceLock::new();
    PAM.get_or_init(|| unsafe {
        let lib = libc::dlopen(c"libpam.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if lib.is_null() {
            return None;
        }
        let symbol = |name: &CStr| {
            let ptr = libc::dlsym(lib, name.as_ptr());
            (!ptr.is_null()).then_some(ptr)
        };
        Some(Pam {
            start: std::mem::transmute::<*mut c_void, Start>(symbol(c"pam_start")?),
            authenticate: std::mem::transmute::<*mut c_void, Authenticate>(symbol(c"pam_authenticate")?),
            end: std::mem::transmute::<*mut c_void, End>(symbol(c"pam_end")?),
            strerror: std::mem::transmute::<*mut c_void, StrError>(symbol(c"pam_strerror")?),
        })
    })
    .as_ref()
}

/// Answers PAM's password prompts with the password; anything else it asks is refused.
unsafe extern "C" fn converse(
    count: c_int,
    messages: *mut *const Message,
    responses: *mut *mut Response,
    data: *mut c_void,
) -> c_int {
    if count <= 0 || messages.is_null() || responses.is_null() {
        return PAM_CONV_ERR;
    }
    let count = count as usize;
    // PAM frees the replies with free(), so they come from malloc.
    let replies = unsafe { libc::calloc(count, std::mem::size_of::<Response>()) } as *mut Response;
    if replies.is_null() {
        return PAM_BUF_ERR;
    }
    let password = data as *const c_char;
    for i in 0..count {
        let message = unsafe { &**messages.add(i) };
        let reply = unsafe { &mut *replies.add(i) };
        match message.style {
            PAM_PROMPT_ECHO_OFF => {
                reply.resp = unsafe { libc::strdup(password) };
                if reply.resp.is_null() {
                    unsafe { free_replies(replies, i) };
                    return PAM_BUF_ERR;
                }
            }
            PAM_ERROR_MSG | PAM_TEXT_INFO => {}
            _ => {
                unsafe { free_replies(replies, i) };
                return PAM_CONV_ERR;
            }
        }
    }
    unsafe { *responses = replies };
    PAM_SUCCESS
}

/// Frees the first `filled` replies (wiping any password in them) and the array.
unsafe fn free_replies(replies: *mut Response, filled: usize) {
    for i in 0..filled {
        let resp = unsafe { (*replies.add(i)).resp };
        if !resp.is_null() {
            unsafe {
                std::ptr::write_bytes(resp, 0, libc::strlen(resp));
                libc::free(resp.cast());
            }
        }
    }
    unsafe { libc::free(replies.cast()) };
}

fn user_name() -> Option<CString> {
    let entry = unsafe { libc::getpwuid(libc::getuid()) };
    if entry.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr((*entry).pw_name) }.to_owned())
}

/// Checks `password` against the current user's login password. Blocks (PAM may wait a few
/// seconds after a wrong password), so call it off the main thread. The password is wiped
/// from memory afterwards.
pub fn check_password(password: String) -> Outcome {
    let mut bytes = password.into_bytes();
    let outcome = check(&bytes);
    bytes.fill(0);
    outcome
}

fn check(password: &[u8]) -> Outcome {
    if password.is_empty() || password.contains(&0) {
        return Outcome::Wrong;
    }
    let Some(pam) = pam() else {
        return Outcome::Unavailable("PAM (libpam.so.0) isn't installed".into());
    };
    let Some(user) = user_name() else {
        return Outcome::Unavailable("your user account wasn't found".into());
    };
    let mut secret = Vec::with_capacity(password.len() + 1);
    secret.extend_from_slice(password);
    secret.push(0);

    let conv = Conv { conv: converse, data: secret.as_ptr() as *mut c_void };
    let mut handle = std::ptr::null_mut();
    let outcome = unsafe {
        let started = (pam.start)(SERVICE.as_ptr(), user.as_ptr(), &conv, &mut handle);
        if started != PAM_SUCCESS || handle.is_null() {
            Outcome::Unavailable(format!("PAM couldn't start (error {started})"))
        } else {
            let result = (pam.authenticate)(handle, PAM_DISALLOW_NULL_AUTHTOK);
            let outcome = match result {
                PAM_SUCCESS => Outcome::Correct,
                PAM_AUTH_ERR | PAM_MAXTRIES => Outcome::Wrong,
                other => {
                    let text = (pam.strerror)(handle, other);
                    let text = if text.is_null() {
                        format!("error {other}")
                    } else {
                        CStr::from_ptr(text).to_string_lossy().into_owned()
                    };
                    Outcome::Unavailable(text)
                }
            };
            (pam.end)(handle, result);
            outcome
        }
    };
    secret.fill(0);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Not tested with a real wrong password: each failure counts towards the system's lockout
    // (pam_faillock) and is logged as a failed login.
    fn empty_password_is_refused_without_asking_pam() {
        assert_eq!(check_password(String::new()), Outcome::Wrong);
        assert_eq!(check_password("a\0b".into()), Outcome::Wrong);
    }
}
