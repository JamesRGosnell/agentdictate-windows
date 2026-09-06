use std::{io, os::windows::ffi::OsStrExt, path::Path};
use windows_sys::Win32::{
    Foundation::*, Security::Authorization::*, Security::*, System::Threading::*,
};
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);
impl SecurityDescriptor {
    pub fn current_user() -> io::Result<Self> {
        // SAFETY: all API output buffers are checked before use and released below.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut bytes = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut bytes);
            let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            );
            CloseHandle(token);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            let mut sid = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0;
            while *sid.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
            LocalFree(sid.cast());
            let sddl: Vec<u16> = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{text})")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }
    }
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}
impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
pub fn restrict_path(path: &Path) -> io::Result<()> {
    let security = SecurityDescriptor::current_user()?;
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        SetFileSecurityW(
            name.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            security.0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_files_have_a_protected_dacl_for_only_system_and_the_current_user() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private.json");
        crate::write_atomic(&path, b"fixture", 0o600).unwrap();
        unsafe {
            let expected = SecurityDescriptor::current_user().unwrap();
            let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut size = 0;
            GetFileSecurityW(
                name.as_ptr(),
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut size,
            );
            let mut bytes = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            assert_ne!(
                GetFileSecurityW(
                    name.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    bytes.as_mut_ptr().cast(),
                    size,
                    &mut size
                ),
                0
            );
            let render = |descriptor| {
                let mut text = std::ptr::null_mut();
                assert_ne!(
                    ConvertSecurityDescriptorToStringSecurityDescriptorW(
                        descriptor,
                        1,
                        DACL_SECURITY_INFORMATION,
                        &mut text,
                        std::ptr::null_mut()
                    ),
                    0
                );
                let mut len = 0;
                while *text.add(len) != 0 {
                    len += 1;
                }
                let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
                LocalFree(text.cast());
                value
            };
            let actual = render(bytes.as_mut_ptr().cast());
            let intended = render(expected.0);
            assert_eq!(actual, intended);
            assert!(actual.starts_with("D:P"));
        }
    }
}
