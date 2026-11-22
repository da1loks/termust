use std::env;
use std::io::{self, Write};
use std::ptr::{null, null_mut};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use uuid::Uuid;
use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::Security::Credentials::{
    CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_PERSIST_SESSION,
    CRED_TYPE_GENERIC, CREDENTIALW, CREDUI_FLAGS_ALWAYS_SHOW_UI, CREDUI_FLAGS_DO_NOT_PERSIST,
    CREDUI_FLAGS_GENERIC_CREDENTIALS, CREDUI_FLAGS_KEEP_USERNAME, CREDUI_INFOW, CredDeleteW,
    CredFree, CredReadW, CredUIPromptForCredentialsW, CredWriteW,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_TOPMOST, MB_YESNO, MessageBoxW,
};
use zeroize::{Zeroize, Zeroizing};

const ID_ENV: &str = "TERMUST_SSH_CREDENTIAL_ID";
const PROMPT_ENV: &str = "TERMUST_SSH_PASSWORD_PROMPT";

#[derive(Clone, Debug)]
pub struct Askpass {
    id: Option<Uuid>,
    prompt: String,
    _temporary: Option<Arc<TemporaryPassword>>,
}

#[derive(Debug)]
struct TemporaryPassword(Uuid);

impl Drop for TemporaryPassword {
    fn drop(&mut self) {
        let _ = remove(self.0);
    }
}

impl Askpass {
    pub fn manual() -> Self {
        Self {
            id: None,
            prompt: String::new(),
            _temporary: None,
        }
    }

    pub fn saved(id: Uuid, prompt: String) -> Self {
        Self {
            id: Some(id),
            prompt,
            _temporary: None,
        }
    }

    pub fn temporary(password: &str, prompt: String) -> Result<Self> {
        let id = store(password, CRED_PERSIST_SESSION)?;
        Ok(Self {
            id: Some(id),
            prompt,
            _temporary: Some(Arc::new(TemporaryPassword(id))),
        })
    }

    pub fn environment(&self) -> Result<[(String, String); 5]> {
        let exe = env::current_exe().context("Путь к Termust недоступен")?;
        let exe = exe.to_str().context("Некорректный путь к Termust")?;
        Ok([
            ("SSH_ASKPASS".into(), exe.into()),
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            ("SSH_ASKPASS_PROMPT".into(), String::new()),
            (
                ID_ENV.into(),
                self.id.map_or_else(String::new, |id| id.to_string()),
            ),
            (PROMPT_ENV.into(), self.prompt.clone()),
        ])
    }
}

pub fn save(password: &str) -> Result<Uuid> {
    store(password, CRED_PERSIST_LOCAL_MACHINE)
}

fn store(password: &str, persistence: u32) -> Result<Uuid> {
    validate(password.as_bytes())?;
    let id = Uuid::new_v4();
    let mut name = target(id);
    let mut bytes = Zeroizing::new(password.as_bytes().to_vec());
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: name.as_mut_ptr(),
        CredentialBlobSize: bytes.len() as u32,
        CredentialBlob: bytes.as_mut_ptr(),
        Persist: persistence,
        ..Default::default()
    };
    // All fields point to buffers that live through the synchronous Windows API call.
    if unsafe { CredWriteW(&credential, 0) } == 0 {
        return Err(io::Error::last_os_error()).context("Не удалось сохранить пароль в Windows");
    }
    Ok(id)
}

pub fn remove(id: Uuid) -> Result<()> {
    let name = target(id);
    if unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_NOT_FOUND as i32) {
            return Err(error).context("Не удалось удалить сохранённый пароль из Windows");
        }
    }
    Ok(())
}

fn read(id: Uuid) -> Result<Zeroizing<Vec<u8>>> {
    let name = target(id);
    let mut credential = null_mut();
    if unsafe { CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 {
        return Err(io::Error::last_os_error()).context("Пароль недоступен в хранилище Windows");
    }
    // CredReadW owns the contiguous buffer until CredFree.
    let value = unsafe {
        let credential_ref = &*credential;
        let len = credential_ref.CredentialBlobSize;
        let value = if len != 0 && len <= CRED_MAX_CREDENTIAL_BLOB_SIZE {
            let blob = std::slice::from_raw_parts_mut(credential_ref.CredentialBlob, len as usize);
            let value = Zeroizing::new(blob.to_vec());
            blob.zeroize();
            value
        } else {
            Zeroizing::new(Vec::new())
        };
        CredFree(credential.cast());
        value
    };
    validate(&value)?;
    Ok(value)
}

fn validate(password: &[u8]) -> Result<()> {
    ensure!(
        !password.is_empty()
            && password.len() <= 1023
            && !password
                .iter()
                .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n')),
        "Пароль должен занимать от 1 до 1023 байт и помещаться в одну строку"
    );
    Ok(())
}

pub fn answer_ssh_prompt() -> Option<i32> {
    let id = env::var(ID_ENV).ok()?;
    let result = if id.is_empty() {
        answer(None)
    } else {
        Uuid::parse_str(&id)
            .context("Неверный идентификатор пароля")
            .and_then(|id| answer(Some(id)))
    };
    Some(i32::from(result.is_err()))
}

fn answer(id: Option<Uuid>) -> Result<()> {
    let prompt = env::args().nth(1).context("Запрос OpenSSH недоступен")?;
    let expected = env::var(PROMPT_ENV).context("Пароль не привязан к запросу OpenSSH")?;
    // OpenSSH reads the reply from a pipe. Never emit a credential to a console.
    ensure!(
        unsafe { GetFileType(GetStdHandle(STD_OUTPUT_HANDLE)) } == FILE_TYPE_PIPE,
        "Для ответа нужен канал OpenSSH"
    );

    let mode = env::var("SSH_ASKPASS_PROMPT").unwrap_or_default();
    let mut output = io::stdout().lock();
    if let Some(id) = id
        && prompt.trim_end() == expected
    {
        let password = read(id).or_else(|_| prompt_password(&prompt))?;
        output.write_all(&password)?;
    } else if mode == "none" {
        return Ok(());
    } else if mode == "confirm" || prompt.contains("Are you sure you want to continue connecting") {
        let message = wide(&prompt);
        let title = wide("Termust · Подтверждение SSH");
        let reply = unsafe {
            MessageBoxW(
                null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_YESNO | MB_DEFBUTTON2 | MB_ICONQUESTION | MB_TOPMOST,
            )
        };
        output.write_all(if reply == IDYES { b"yes" } else { b"no" })?;
    } else {
        let password = prompt_password(&prompt)?;
        output.write_all(&password)?;
    }
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn prompt_password(prompt: &str) -> Result<Zeroizing<Vec<u8>>> {
    let title = wide("Termust · SSH");
    let message = wide(prompt);
    let info = CREDUI_INFOW {
        cbSize: std::mem::size_of::<CREDUI_INFOW>() as u32,
        pszCaptionText: title.as_ptr(),
        pszMessageText: message.as_ptr(),
        ..Default::default()
    };
    let mut user = [0_u16; 256];
    user[..3].copy_from_slice(&[b'S' as u16, b'S' as u16, b'H' as u16]);
    let mut value = Zeroizing::new([0_u16; 1024]);
    let mut save = 0;
    let result = unsafe {
        CredUIPromptForCredentialsW(
            &info,
            title.as_ptr(),
            null(),
            0,
            user.as_mut_ptr(),
            user.len() as u32,
            value.as_mut_ptr(),
            value.len() as u32,
            &mut save,
            CREDUI_FLAGS_GENERIC_CREDENTIALS
                | CREDUI_FLAGS_KEEP_USERNAME
                | CREDUI_FLAGS_ALWAYS_SHOW_UI
                | CREDUI_FLAGS_DO_NOT_PERSIST,
        )
    };
    ensure!(result == ERROR_SUCCESS, "Ввод пароля отменён");
    let len = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    let secret = Zeroizing::new(String::from_utf16(&value[..len])?.into_bytes());
    validate(&secret)?;
    Ok(secret)
}

fn target(id: Uuid) -> Vec<u16> {
    wide(&format!("Termust/SSH/{id}"))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
