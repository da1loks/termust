use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::Ipv6Addr;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::credentials::Askpass;

#[derive(Clone, Debug)]
pub struct Launch {
    pub title: String,
    pub transport: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub askpass: Option<Askpass>,
}

#[derive(Clone, Copy)]
pub enum LocalShell {
    PowerShell,
    Cmd,
    Wsl,
}

impl LocalShell {
    pub const ALL: [Self; 3] = [Self::PowerShell, Self::Cmd, Self::Wsl];

    pub fn label(self) -> &'static str {
        match self {
            Self::PowerShell => "PowerShell",
            Self::Cmd => "CMD",
            Self::Wsl => "WSL",
        }
    }

    pub fn launch(self) -> Result<Launch> {
        let (relative_path, args): (&str, &[&str]) = match self {
            Self::PowerShell => ("WindowsPowerShell/v1.0/powershell.exe", &["-NoLogo"]),
            Self::Cmd => ("cmd.exe", &[]),
            Self::Wsl => ("wsl.exe", &[]),
        };
        let program = system_program(relative_path)
            .with_context(|| format!("Не удалось найти {} в Windows", self.label()))?;
        Ok(Launch {
            title: self.label().into(),
            transport: "Локально",
            program,
            args: args.iter().map(|arg| (*arg).into()).collect(),
            askpass: None,
        })
    }
}

#[derive(Clone, Copy)]
pub enum Protocol {
    Ssh,
    Sftp,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ssh => "SSH",
            Self::Sftp => "SFTP",
        }
    }

    pub fn launch(self, profile: &Profile) -> Result<Launch> {
        self.launch_with_password(profile, "")
    }

    pub fn launch_with_password(self, profile: &Profile, password: &str) -> Result<Launch> {
        let program = system_program("OpenSSH/ssh.exe").with_context(|| {
            format!(
                "Для {} нужен компонент Windows «Клиент OpenSSH»",
                self.label()
            )
        })?;

        let mut args = profile.client_args()?;
        if matches!(self, Self::Sftp) {
            args.splice(
                0..0,
                [
                    "-T".into(),
                    "-s".into(),
                    "-o".into(),
                    "ClearAllForwardings=yes".into(),
                    "-o".into(),
                    "PermitLocalCommand=no".into(),
                    "-o".into(),
                    "RemoteCommand=none".into(),
                ],
            );
            args.push("sftp".into());
        }
        let askpass = if !password.is_empty() {
            Some(Askpass::temporary(
                password,
                profile.effective_endpoint()?.password_prompt(),
            )?)
        } else if let Some(saved) = &profile.password {
            let endpoint = profile.effective_endpoint()?;
            ensure!(
                endpoint == saved.endpoint,
                "Назначение SSH изменилось. Введи новый пароль или выбери ручной ввод"
            );
            Some(Askpass::saved(saved.id, endpoint.password_prompt()))
        } else {
            None
        };
        if askpass.is_some() {
            // Only OpenSSH's own password prompt may receive the saved password. A
            // server controls the text of keyboard-interactive challenges.
            args.splice(
                0..0,
                [
                    "-o".into(),
                    "KbdInteractiveAuthentication=no".into(),
                    "-o".into(),
                    "NumberOfPasswordPrompts=1".into(),
                ],
            );
        }

        Ok(Launch {
            title: profile.label().into(),
            transport: self.label(),
            program,
            args,
            askpass,
        })
    }
}

fn system_program(relative_path: &str) -> Result<PathBuf> {
    let root = PathBuf::from(env::var_os("SystemRoot").context("SystemRoot недоступен")?);
    let program = root.join("System32").join(relative_path);
    ensure!(
        program.is_absolute() && program.is_file(),
        "{}",
        program.display()
    );
    Ok(program)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: Option<u16>,
    pub identity_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<SavedPassword>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPassword {
    pub id: Uuid,
    pub endpoint: Endpoint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    host: String,
    user: String,
    port: u16,
    host_key_alias: Option<String>,
}

impl Endpoint {
    fn password_prompt(&self) -> String {
        let host = self.host_key_alias.as_deref().unwrap_or(&self.host);
        format!("{}@{host}'s password:", self.user)
    }
}

impl Profile {
    pub fn same_login(&self, other: &Self) -> bool {
        self.host.eq_ignore_ascii_case(&other.host)
            && self.user == other.user
            && self.port == other.port
    }

    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.host
        } else {
            &self.name
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.validated_host()?;
        self.validate_options()
    }

    fn client_args(&self) -> Result<Vec<String>> {
        let host = self.validated_host()?;
        self.validate_options()?;
        let mut args = Vec::new();
        if let Some(port) = self.port {
            args.extend(["-p".into(), port.to_string()]);
        }
        if !self.user.is_empty() {
            args.extend(["-o".into(), format!("User={}", self.user)]);
        }
        if !self.identity_file.is_empty() {
            args.extend(["-i".into(), self.identity_file.clone()]);
        }
        args.extend(["--".into(), host.into()]);
        Ok(args)
    }

    pub fn effective_endpoint(&self) -> Result<Endpoint> {
        let mut command = Command::new(
            system_program("OpenSSH/ssh.exe")
                .context("Для сохранения пароля нужен клиент OpenSSH")?,
        );
        command
            .creation_flags(CREATE_NO_WINDOW)
            .arg("-G")
            .args(self.client_args()?)
            .stdin(Stdio::null());
        if let Some(home) = env::var_os("USERPROFILE").filter(|path| Path::new(path).is_dir()) {
            command.current_dir(home);
        }
        let output = command
            .output()
            .context("Не удалось прочитать настройки OpenSSH")?;
        ensure!(
            output.status.success(),
            "Не удалось прочитать настройки OpenSSH для этого адреса"
        );
        let config = String::from_utf8(output.stdout).context("Некорректные настройки OpenSSH")?;
        let get = |key| {
            config.lines().find_map(|line| {
                let (name, value) = line.split_once(' ')?;
                (name == key)
                    .then(|| value.trim())
                    .filter(|value| !value.is_empty())
            })
        };
        Ok(Endpoint {
            host: get("hostname")
                .context("OpenSSH не вернул адрес сервера")?
                .into(),
            user: get("user")
                .context("OpenSSH не вернул имя пользователя")?
                .into(),
            port: get("port").context("OpenSSH не вернул порт")?.parse()?,
            host_key_alias: get("hostkeyalias").map(str::to_owned),
        })
    }

    fn validated_host(&self) -> Result<&str> {
        let host = self.host.as_str();
        let address = host
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(host);
        if address.parse::<Ipv6Addr>().is_ok() {
            return Ok(address);
        }
        if !host.is_empty()
            && !host.starts_with('-')
            && host.len() <= 253
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Ok(host);
        }
        bail!("Укажи DNS-имя, IP-адрес или алиас из .ssh/config");
    }

    fn validate_options(&self) -> Result<()> {
        ensure!(self.port != Some(0), "Порт должен быть от 1 до 65535");
        ensure!(
            self.user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-@".contains(&byte)),
            "Имя пользователя может содержать латинские буквы, цифры и . _ - @"
        );
        ensure!(
            !self.identity_file.chars().any(char::is_control),
            "Путь к ключу содержит управляющий символ"
        );
        Ok(())
    }
}

pub struct Profiles {
    path: PathBuf,
    pub items: Vec<Profile>,
}

impl Profiles {
    pub fn load() -> Result<Self> {
        let local = env::var_os("LOCALAPPDATA").context("LOCALAPPDATA недоступен")?;
        let path = PathBuf::from(local).join("termust").join("profiles.json");
        let items = read_profiles(&path)?;
        Ok(Self { path, items })
    }

    pub fn replace(&mut self, items: Vec<Profile>) -> Result<()> {
        let directory = self.path.parent().context("Неверный путь к профилям")?;
        fs::create_dir_all(directory).context("Не удалось создать папку профилей")?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("profiles.lock"))
            .context("Не удалось открыть файл блокировки профилей")?;
        lock.try_lock()
            .context("Профили уже сохраняются в другом окне")?;
        ensure!(
            read_profiles(&self.path)? == self.items,
            "Профили изменились в другом окне. Открой приложение заново"
        );

        let mut file = tempfile::NamedTempFile::new_in(directory)
            .context("Не удалось создать временный файл профилей")?;
        serde_json::to_writer_pretty(&mut file, &items)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(&self.path)
            .context("Не удалось сохранить профили")?;
        self.items = items;
        Ok(())
    }
}

fn read_profiles(path: &Path) -> Result<Vec<Profile>> {
    match File::open(path) {
        Ok(file) => serde_json::from_reader(io::BufReader::new(file))
            .with_context(|| format!("Ошибка чтения {}", path.display())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("Не удалось открыть {}", path.display())),
    }
}
