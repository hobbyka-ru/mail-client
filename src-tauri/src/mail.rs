use crate::cache::{self, CachedPage};
use async_imap::{imap_proto::types::BodyStructure, types::Flag, Client, Session};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::StreamExt;
use keyring::Entry;
use lettre::{
    message::{
        header::ContentType, Attachment as LettreAttachment, Mailbox, MultiPart, SinglePart,
    },
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use mail_parser::{MessageParser, MimeHeaders};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tauri::{AppHandle, Emitter, State};
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;

const APP_KEYRING_SERVICE: &str = "ru.hobbyka.yandex-mail.v2";
const CLI_KEYRING_SERVICE: &str = "email-cli";
const LABEL_PREFIX: &str = "emailcli-";
const TIMEOUT: Duration = Duration::from_secs(30);
const IMAGE_PREVIEW_LIMIT: usize = 4 * 1024 * 1024;

type ImapSession = Session<TlsStream<TcpStream>>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    email: String,
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    name: String,
    path: String,
    raw_path: String,
    special_use: Option<String>,
    total: u32,
    unseen: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    pub(crate) uid: u32,
    pub(crate) subject: String,
    pub(crate) from_name: String,
    pub(crate) from_address: String,
    pub(crate) date: i64,
    pub(crate) seen: bool,
    pub(crate) flagged: bool,
    pub(crate) labels: Vec<String>,
    pub(crate) size: u32,
    pub(crate) has_attachments: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    pub(crate) items: Vec<MessageSummary>,
    pub(crate) total: u32,
    pub(crate) unseen: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDetail {
    #[serde(flatten)]
    pub(crate) summary: MessageSummary,
    pub(crate) to: String,
    pub(crate) body_text: Option<String>,
    pub(crate) body_html: Option<String>,
    pub(crate) attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Label {
    pub(crate) name: String,
    pub(crate) count: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub(crate) id: u32,
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    pub(crate) size: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalAttachment {
    path: String,
    filename: String,
    size: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentPreview {
    id: u32,
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MessageSelection {
    Uids {
        uids: Vec<u32>,
    },
    All {
        label: Option<String>,
        query: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum MessageAction {
    Seen { enabled: bool },
    Flagged { enabled: bool },
    Label { name: String, enabled: bool },
    Move { destination: String },
    Delete,
    Archive,
    Spam,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    affected: usize,
}

#[derive(Default)]
pub struct WatchState(Mutex<Option<tauri::async_runtime::JoinHandle<()>>>);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutgoingMessage {
    to: String,
    subject: String,
    text: String,
    html: Option<String>,
    attachments: Vec<String>,
}

fn config_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(".config/email-cli/account.json"))
        .ok_or_else(|| "Не удалось определить домашнюю папку".to_string())
}

fn read_account() -> Result<Account, String> {
    serde_json::from_str(
        &fs::read_to_string(config_path()?).map_err(|_| "Почта ещё не настроена".to_string())?,
    )
    .map_err(|_| "Настройки почты повреждены".to_string())
}

fn load_account() -> Result<(Account, String), String> {
    let account = read_account()?;
    let app_entry = Entry::new(APP_KEYRING_SERVICE, &account.email)
        .map_err(|_| "Не удалось открыть хранилище паролей".to_string())?;
    let password = match app_entry.get_password() {
        Ok(password) => password,
        Err(keyring::Error::NoEntry) => {
            let password = Entry::new(CLI_KEYRING_SERVICE, &account.email)
                .map_err(|_| "Не удалось открыть хранилище паролей".to_string())?
                .get_password()
                .map_err(|_| "Пароль приложения не найден".to_string())?;
            // The CLI-created macOS item can prompt on every rebuilt app. Keep a
            // separate native item for the app and use the old one only to migrate.
            let _ = app_entry.set_password(&password);
            password
        }
        Err(_) => return Err("Пароль приложения не найден".to_string()),
    };
    Ok((account, password))
}

pub fn import_password_from_stdin() -> Result<(), String> {
    use std::io::Read;

    let account = read_account()?;
    let mut password = String::new();
    std::io::stdin()
        .read_to_string(&mut password)
        .map_err(|_| "Не удалось прочитать пароль".to_string())?;
    let password = password.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        return Err("Пароль не передан".to_string());
    }
    Entry::new(APP_KEYRING_SERVICE, &account.email)
        .map_err(|_| "Не удалось открыть хранилище паролей".to_string())?
        .set_password(password)
        .map_err(|_| "Не удалось сохранить пароль".to_string())
}

async fn connect_with(
    account: Account,
    password: String,
) -> Result<(Account, ImapSession), String> {
    let tcp = tokio::time::timeout(
        TIMEOUT,
        TcpStream::connect((account.imap_host.as_str(), account.imap_port)),
    )
    .await
    .map_err(|_| "Сервер почты не ответил вовремя".to_string())?
    .map_err(|error| format!("Не удалось подключиться к почте: {error}"))?;
    let tls = native_tls::TlsConnector::builder()
        .build()
        .map_err(|error| format!("Не удалось подготовить защищённое соединение: {error}"))?;
    let stream = tokio::time::timeout(
        TIMEOUT,
        tokio_native_tls::TlsConnector::from(tls).connect(&account.imap_host, tcp),
    )
    .await
    .map_err(|_| "Защищённое соединение не ответило вовремя".to_string())?
    .map_err(|error| format!("Ошибка защищённого соединения: {error}"))?;
    let client = Client::new(stream);
    let session = tokio::time::timeout(TIMEOUT, client.login(&account.email, password))
        .await
        .map_err(|_| "Вход в почту не завершился вовремя".to_string())?
        .map_err(|(error, _)| format!("Не удалось войти в почту: {error}"))?;
    Ok((account, session))
}

async fn connect() -> Result<(Account, ImapSession), String> {
    let (account, password) = load_account()?;
    connect_with(account, password).await
}

fn replace_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    fs::create_dir_all(path.parent().ok_or("Некорректный путь настроек")?)
        .map_err(|_| "Не удалось создать папку настроек".to_string())?;
    let temporary = path.with_extension("new");
    let backup = path.with_extension("old");
    fs::write(&temporary, contents).map_err(|_| "Не удалось сохранить настройки".to_string())?;
    let _ = fs::remove_file(&backup);
    if path.exists() {
        fs::rename(path, &backup).map_err(|_| "Не удалось заменить настройки".to_string())?;
    }
    if fs::rename(&temporary, path).is_err() {
        if backup.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err("Не удалось сохранить настройки".to_string());
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

fn write_account(account: &Account) -> Result<(), String> {
    replace_file(
        &config_path()?,
        &serde_json::to_vec_pretty(account).map_err(|error| error.to_string())?,
    )
}

fn user_label(flag: Flag<'_>) -> Option<String> {
    let Flag::Custom(value) = flag else {
        return None;
    };
    let raw = value.as_ref();
    if raw.starts_with(LABEL_PREFIX) {
        return URL_SAFE_NO_PAD
            .decode(&raw[LABEL_PREFIX.len()..])
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
    }
    let internal = raw == "encrypted"
        || raw.starts_with("system_")
        || matches!(
            raw,
            "$Junk" | "$NotJunk" | "Junk" | "NotJunk" | "JunkRecorded" | "$Forwarded"
        );
    (!internal).then(|| raw.to_string())
}

fn encode_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 15 || label.contains(['\r', '\n']) {
        return Err("Название метки должно содержать от 1 до 15 символов".to_string());
    }
    if label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_.$-".contains(c))
    {
        Ok(label.to_string())
    } else {
        Ok(format!(
            "{LABEL_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(label.as_bytes())
        ))
    }
}

fn bodystructure_has_attachments(body: &BodyStructure<'_>) -> bool {
    let common = match body {
        BodyStructure::Basic { common, .. }
        | BodyStructure::Text { common, .. }
        | BodyStructure::Message { common, .. }
        | BodyStructure::Multipart { common, .. } => common,
    };
    let named = common.ty.params.as_ref().is_some_and(|params| {
        params
            .iter()
            .any(|(key, _)| matches!(key.to_ascii_lowercase().as_str(), "name" | "filename"))
    }) || common.disposition.as_ref().is_some_and(|disposition| {
        disposition.params.as_ref().is_some_and(|params| {
            params
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("filename"))
        })
    });
    let attached = common
        .disposition
        .as_ref()
        .is_some_and(|disposition| disposition.ty.eq_ignore_ascii_case("attachment"));
    attached
        || named
        || matches!(body, BodyStructure::Basic { common, .. } if !common.ty.ty.eq_ignore_ascii_case("text"))
        || matches!(body, BodyStructure::Multipart { bodies, .. } if bodies.iter().any(bodystructure_has_attachments))
}

fn parse_summary(
    fetch: &async_imap::types::Fetch,
    raw_header: &[u8],
) -> Result<MessageSummary, String> {
    let message = MessageParser::default()
        .parse(raw_header)
        .ok_or_else(|| "Не удалось разобрать заголовок письма".to_string())?;
    let first = message.from().and_then(|address| address.first());
    let flags: Vec<_> = fetch.flags().collect();
    Ok(MessageSummary {
        uid: fetch
            .uid
            .ok_or_else(|| "Сервер не вернул UID письма".to_string())?,
        subject: message.subject().unwrap_or("Без темы").to_string(),
        from_name: first
            .and_then(|address| address.name.as_deref())
            .unwrap_or("")
            .to_string(),
        from_address: first
            .and_then(|address| address.address.as_deref())
            .unwrap_or("")
            .to_string(),
        date: message
            .date()
            .map(|date| date.to_timestamp())
            .or_else(|| fetch.internal_date().map(|date| date.timestamp()))
            .unwrap_or_default(),
        seen: flags.iter().any(|flag| matches!(flag, Flag::Seen)),
        flagged: flags.iter().any(|flag| matches!(flag, Flag::Flagged)),
        labels: flags.into_iter().filter_map(user_label).collect(),
        size: fetch.size.unwrap_or_default(),
        has_attachments: fetch
            .bodystructure()
            .is_some_and(bodystructure_has_attachments),
    })
}

async fn fetch_summaries(
    session: &mut ImapSession,
    raw_path: &str,
    uids: &[u32],
) -> Result<Vec<MessageSummary>, String> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    session
        .select(raw_path)
        .await
        .map_err(|error| format!("Не удалось открыть папку: {error}"))?;
    let set = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let stream = session
        .uid_fetch(
            set,
            "(UID FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER])",
        )
        .await
        .map_err(|error| format!("Не удалось получить письма: {error}"))?;
    let fetched = stream.collect::<Vec<_>>().await;
    let mut messages = fetched
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|fetch| {
            let header = fetch.header()?.to_vec();
            parse_summary(&fetch, &header).ok()
        })
        .collect::<Vec<_>>();
    messages.sort_by_key(|message| std::cmp::Reverse(message.uid));
    Ok(messages)
}

async fn fetch_latest_summaries(
    session: &mut ImapSession,
    exists: u32,
    limit: usize,
) -> Result<Vec<MessageSummary>, String> {
    if exists == 0 {
        return Ok(Vec::new());
    }
    let count = limit.clamp(1, 100) as u32;
    let start = exists.saturating_sub(count).saturating_add(1).max(1);
    let stream = session
        .fetch(
            format!("{start}:{exists}"),
            "(UID FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER])",
        )
        .await
        .map_err(|error| format!("Не удалось получить письма: {error}"))?;
    let fetched = stream.collect::<Vec<_>>().await;
    let mut messages = fetched
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|fetch| {
            let header = fetch.header()?.to_vec();
            parse_summary(&fetch, &header).ok()
        })
        .collect::<Vec<_>>();
    messages.sort_by_key(|message| std::cmp::Reverse(message.uid));
    Ok(messages)
}

#[tauri::command]
pub fn get_account() -> Result<Account, String> {
    read_account()
}

#[tauri::command]
pub async fn replace_account(account: Account, password: String) -> Result<(), String> {
    if account.email.trim().is_empty() || password.is_empty() {
        return Err("Укажите адрес и пароль приложения".to_string());
    }
    let previous = load_account().ok();
    let (_, mut candidate) = connect_with(account.clone(), password.clone()).await?;
    candidate
        .logout()
        .await
        .map_err(|error| format!("Не удалось завершить проверку аккаунта: {error}"))?;

    Entry::new(APP_KEYRING_SERVICE, &account.email)
        .map_err(|_| "Не удалось открыть хранилище паролей".to_string())?
        .set_password(&password)
        .map_err(|_| "Не удалось сохранить пароль".to_string())?;
    if let Err(error) = Entry::new(CLI_KEYRING_SERVICE, &account.email)
        .and_then(|entry| entry.set_password(&password))
    {
        let _ = Entry::new(APP_KEYRING_SERVICE, &account.email)
            .and_then(|entry| entry.delete_credential());
        return Err(format!(
            "Не удалось сохранить пароль для email-cli: {error}"
        ));
    }
    if let Err(error) = write_account(&account) {
        if let Some((_, old_password)) = previous
            .as_ref()
            .filter(|(old, _)| old.email == account.email)
        {
            let _ = Entry::new(APP_KEYRING_SERVICE, &account.email)
                .and_then(|entry| entry.set_password(old_password));
            let _ = Entry::new(CLI_KEYRING_SERVICE, &account.email)
                .and_then(|entry| entry.set_password(old_password));
        } else {
            let _ = Entry::new(APP_KEYRING_SERVICE, &account.email)
                .and_then(|entry| entry.delete_credential());
            let _ = Entry::new(CLI_KEYRING_SERVICE, &account.email)
                .and_then(|entry| entry.delete_credential());
        }
        return Err(error);
    }
    if let Some((previous, _)) = previous.filter(|(old, _)| old.email != account.email) {
        cache::purge(&previous.email);
        let _ = Entry::new(APP_KEYRING_SERVICE, &previous.email)
            .and_then(|entry| entry.delete_credential());
        let _ = Entry::new(CLI_KEYRING_SERVICE, &previous.email)
            .and_then(|entry| entry.delete_credential());
    }
    Ok(())
}

#[tauri::command]
pub async fn test_account() -> Result<(), String> {
    let (_, mut session) = connect().await?;
    session.logout().await.map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn list_folders() -> Result<Vec<Folder>, String> {
    let (account, mut session) = connect().await?;
    let stream = session
        .list(Some(""), Some("*"))
        .await
        .map_err(|error| error.to_string())?;
    let names = stream.collect::<Vec<_>>().await;
    let mut folders = Vec::new();
    for name in names.into_iter().filter_map(Result::ok) {
        let raw_path = name.name().to_string();
        let path = utf7_imap::decode_utf7_imap(raw_path.clone());
        let special_use = name
            .attributes()
            .iter()
            .find_map(|attribute| {
                use async_imap::types::NameAttribute;
                match attribute {
                    NameAttribute::Sent => Some("sent"),
                    NameAttribute::Trash => Some("trash"),
                    NameAttribute::Drafts => Some("drafts"),
                    NameAttribute::Junk => Some("junk"),
                    NameAttribute::Archive => Some("archive"),
                    _ => None,
                }
            })
            .map(str::to_string)
            .or_else(|| (path == "INBOX").then(|| "inbox".to_string()));
        folders.push(Folder {
            name: if special_use.as_deref() == Some("inbox") {
                "Входящие".to_string()
            } else {
                path.rsplit(name.delimiter().unwrap_or("/"))
                    .next()
                    .unwrap_or(&path)
                    .to_string()
            },
            path,
            raw_path,
            special_use,
            total: 0,
            unseen: 0,
        });
    }
    folders.sort_by(|a, b| {
        let rank = |folder: &Folder| match folder.special_use.as_deref() {
            Some("inbox") => 0,
            None => 1,
            Some("archive") => 2,
            Some("sent") => 3,
            Some("trash") => 4,
            Some("junk") => 5,
            Some("drafts") => 6,
            _ => 7,
        };
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
    });
    let _ = session.logout().await;
    let _ = cache::write_folders(&account.email, &folders);
    Ok(folders)
}

#[tauri::command]
pub fn get_cached_folders() -> Result<Vec<Folder>, String> {
    let account = read_account()?;
    Ok(cache::read_folders(&account.email))
}

fn folder_name(name: String) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 80 || name.contains(['\r', '\n']) {
        return Err("Введите название папки длиной до 80 символов".to_string());
    }
    Ok(utf7_imap::encode_utf7_imap(name.to_string()))
}

fn editable_folder(email: &str, raw_path: &str) -> Result<(), String> {
    match cache::read_folders(email)
        .iter()
        .find(|folder| folder.raw_path == raw_path)
    {
        Some(folder) if folder.special_use.is_none() => Ok(()),
        Some(_) => Err("Системную папку нельзя изменить".to_string()),
        None => Err("Папка не найдена".to_string()),
    }
}

#[tauri::command]
pub async fn create_folder(name: String) -> Result<Vec<Folder>, String> {
    let encoded = folder_name(name)?;
    let (_, mut session) = connect().await?;
    session
        .create(encoded)
        .await
        .map_err(|error| error.to_string())?;
    let _ = session.logout().await;
    list_folders().await
}

#[tauri::command]
pub async fn rename_folder(raw_path: String, name: String) -> Result<Vec<Folder>, String> {
    let encoded = folder_name(name)?;
    let (account, mut session) = connect().await?;
    editable_folder(&account.email, &raw_path)?;
    session
        .rename(&raw_path, encoded)
        .await
        .map_err(|error| error.to_string())?;
    let _ = session.logout().await;
    list_folders().await
}

#[tauri::command]
pub async fn delete_folder(raw_path: String) -> Result<Vec<Folder>, String> {
    let (account, mut session) = connect().await?;
    editable_folder(&account.email, &raw_path)?;
    session
        .delete(&raw_path)
        .await
        .map_err(|error| error.to_string())?;
    let _ = session.logout().await;
    list_folders().await
}

#[tauri::command]
pub fn get_cached_messages(raw_path: String) -> Result<Option<MessagePage>, String> {
    let account = read_account()?;
    Ok(cache::read_page(&account.email, &raw_path).map(|cached| cached.page))
}

#[tauri::command]
pub fn search_cached_messages(
    raw_path: String,
    label: Option<String>,
    query: String,
) -> Result<MessagePage, String> {
    let account = read_account()?;
    let mut items = cache::search_messages(&account.email, &raw_path, &query);
    if let Some(label) = label.filter(|value| !value.is_empty()) {
        items.retain(|message| message.labels.iter().any(|item| item == &label));
    }
    items.sort_by(|left, right| right.date.cmp(&left.date));
    items.truncate(60);
    Ok(MessagePage {
        total: items.len() as u32,
        unseen: items.iter().filter(|message| !message.seen).count() as u32,
        items,
    })
}

fn message_search(label: Option<&str>, query: Option<&str>) -> Result<String, String> {
    let mut terms = Vec::new();
    if let Some(label) = label.filter(|value| !value.is_empty()) {
        terms.push(format!("KEYWORD {}", encode_label(label)?));
    }
    if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
        if query.contains(['\r', '\n']) {
            return Err("Некорректный поисковый запрос".to_string());
        }
        let value = format!(
            "\"{}\"",
            query.trim().replace('\\', "\\\\").replace('"', "\\\"")
        );
        let fields = ["FROM", "TO", "CC", "BCC", "SUBJECT", "BODY"];
        let mut search = format!("BODY {value}");
        for field in fields[..fields.len() - 1].iter().rev() {
            search = format!("OR {field} {value} {search}");
        }
        terms.push(search);
    }
    Ok(if terms.is_empty() {
        "ALL".to_string()
    } else {
        if query.is_some_and(|value| !value.is_ascii()) {
            format!("CHARSET UTF-8 {}", terms.join(" "))
        } else {
            terms.join(" ")
        }
    })
}

#[tauri::command]
pub async fn list_messages(
    raw_path: String,
    limit: usize,
    label: Option<String>,
    query: Option<String>,
) -> Result<MessagePage, String> {
    let (account, mut session) = connect().await?;
    let unseen = session
        .status(&raw_path, "(UNSEEN)")
        .await
        .ok()
        .and_then(|status| status.unseen)
        .unwrap_or_default();
    let mailbox = session
        .select(&raw_path)
        .await
        .map_err(|error| format!("Не удалось открыть папку: {error}"))?;
    let search = message_search(label.as_deref(), query.as_deref())?;
    let filtered = search != "ALL";
    let (items, total) = if filtered {
        let mut uids = session
            .uid_search(search)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect::<Vec<_>>();
        uids.sort_unstable();
        let start = uids.len().saturating_sub(limit.clamp(1, 100));
        let mut items = fetch_summaries(&mut session, &raw_path, &uids[start..]).await?;
        let mut total = uids.len() as u32;
        if let Some(query) = query.as_deref().filter(|value| !value.trim().is_empty()) {
            for cached in cache::search_messages(&account.email, &raw_path, query) {
                if label
                    .as_deref()
                    .is_some_and(|name| !cached.labels.iter().any(|item| item == name))
                    || items.iter().any(|item| item.uid == cached.uid)
                {
                    continue;
                }
                if uids.binary_search(&cached.uid).is_err() {
                    total += 1;
                }
                items.push(cached);
            }
            items.sort_by(|left, right| right.date.cmp(&left.date));
            items.truncate(limit.clamp(1, 100));
        }
        (items, total)
    } else {
        (
            fetch_latest_summaries(&mut session, mailbox.exists, limit).await?,
            mailbox.exists,
        )
    };
    let _ = session.logout().await;
    let page = MessagePage {
        items,
        total,
        unseen,
    };
    if !filtered {
        let _ = cache::write_page(
            &account.email,
            &raw_path,
            &CachedPage {
                version: cache::CACHE_VERSION,
                uid_validity: mailbox.uid_validity,
                uid_next: mailbox.uid_next,
                page: page.clone(),
            },
        );
    }
    Ok(page)
}

#[tauri::command]
pub async fn get_message(raw_path: String, uid: u32) -> Result<MessageDetail, String> {
    let (account, _) = load_account()?;
    if let Some(detail) = cache::read_detail(&account.email, &raw_path, uid) {
        return Ok(detail);
    }
    let (_, mut session) = connect().await?;
    let mailbox = session
        .select(&raw_path)
        .await
        .map_err(|error| error.to_string())?;
    let stream = session
        .uid_fetch(
            uid.to_string(),
            "(UID FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[])",
        )
        .await
        .map_err(|error| error.to_string())?;
    let fetched = stream.collect::<Vec<_>>().await;
    let fetch = fetched
        .into_iter()
        .find_map(Result::ok)
        .ok_or("Письмо не найдено")?;
    let raw = fetch.body().ok_or("Сервер не вернул письмо")?;
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or("Не удалось разобрать письмо")?;
    let summary = parse_summary(&fetch, raw)?;
    let to = parsed
        .to()
        .map(|addresses| {
            addresses
                .iter()
                .filter_map(|address| address.address.as_deref())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let detail = MessageDetail {
        summary,
        to,
        body_text: parsed.body_text(0).map(|value| value.to_string()),
        body_html: parsed.body_html(0).map(|value| value.to_string()),
        attachments: parsed
            .attachments()
            .enumerate()
            .map(|(id, part)| {
                let mime_type = part
                    .content_type()
                    .map(|value| {
                        format!(
                            "{}/{}",
                            value.c_type,
                            value.c_subtype.as_deref().unwrap_or("octet-stream")
                        )
                    })
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                let extension = match mime_type.as_str() {
                    "image/png" => ".png",
                    "image/jpeg" => ".jpg",
                    "image/gif" => ".gif",
                    "application/pdf" => ".pdf",
                    "text/plain" => ".txt",
                    _ => "",
                };
                Attachment {
                    id: id as u32,
                    filename: part
                        .attachment_name()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Вложение {}{extension}", id + 1)),
                    mime_type,
                    size: part.len(),
                }
            })
            .collect(),
    };
    let _ = session.logout().await;
    let _ = cache::write_detail(&account.email, &raw_path, mailbox.uid_validity, &detail);
    Ok(detail)
}

async fn fetch_raw_message(raw_path: &str, uid: u32) -> Result<Vec<u8>, String> {
    let (_, mut session) = connect().await?;
    session
        .select(raw_path)
        .await
        .map_err(|error| error.to_string())?;
    let stream = session
        .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
        .await
        .map_err(|error| error.to_string())?;
    let fetched = stream.collect::<Vec<_>>().await;
    let raw = fetched
        .into_iter()
        .find_map(Result::ok)
        .and_then(|fetch| fetch.body().map(ToOwned::to_owned))
        .ok_or("Письмо не найдено")?;
    let _ = session.logout().await;
    Ok(raw)
}

fn safe_filename(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .unwrap_or("Вложение")
        .to_string()
}

fn preview_url(mime_type: &str, contents: &[u8]) -> String {
    format!(
        "data:{mime_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(contents)
    )
}

fn write_attachment(raw: &[u8], attachment_id: u32, destination: &Path) -> Result<(), String> {
    let message = MessageParser::default()
        .parse(raw)
        .ok_or("Не удалось разобрать письмо")?;
    let part = message
        .attachment(attachment_id)
        .ok_or("Вложение не найдено")?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("Не удалось создать папку: {error}"))?;
    }
    fs::write(destination, part.contents())
        .map_err(|error| format!("Не удалось сохранить вложение: {error}"))
}

#[tauri::command]
pub async fn save_attachment(
    raw_path: String,
    uid: u32,
    attachment_id: u32,
    destination: String,
) -> Result<String, String> {
    let destination = PathBuf::from(destination);
    let raw = fetch_raw_message(&raw_path, uid).await?;
    write_attachment(&raw, attachment_id, &destination)?;
    Ok(destination.to_string_lossy().to_string())
}

#[tauri::command]
pub async fn open_attachment(
    raw_path: String,
    uid: u32,
    attachment_id: u32,
    filename: String,
) -> Result<String, String> {
    let destination = dirs::cache_dir()
        .ok_or("Не удалось определить папку кеша")?
        .join("yandex-mail")
        .join("opened")
        .join(format!(
            "{uid}-{attachment_id}-{}",
            safe_filename(&filename)
        ));
    let raw = fetch_raw_message(&raw_path, uid).await?;
    write_attachment(&raw, attachment_id, &destination)?;
    Ok(destination.to_string_lossy().to_string())
}

#[tauri::command]
pub async fn prepare_image_previews(
    raw_path: String,
    uid: u32,
) -> Result<Vec<AttachmentPreview>, String> {
    let raw = fetch_raw_message(&raw_path, uid).await?;
    let message = MessageParser::default()
        .parse(&raw)
        .ok_or("Не удалось разобрать письмо")?;
    let mut previews = Vec::new();
    for (id, part) in message.attachments().enumerate() {
        let mime_type = part
            .content_type()
            .map(|value| {
                format!(
                    "{}/{}",
                    value.c_type,
                    value.c_subtype.as_deref().unwrap_or("octet-stream")
                )
            })
            .unwrap_or_default();
        if part.len() > IMAGE_PREVIEW_LIMIT
            || !matches!(
                mime_type.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
            )
        {
            continue;
        }
        previews.push(AttachmentPreview {
            id: id as u32,
            url: preview_url(&mime_type, part.contents()),
        });
    }
    Ok(previews)
}

#[tauri::command]
pub async fn save_all_attachments(
    raw_path: String,
    uid: u32,
    directory: String,
) -> Result<usize, String> {
    let raw = fetch_raw_message(&raw_path, uid).await?;
    let message = MessageParser::default()
        .parse(&raw)
        .ok_or("Не удалось разобрать письмо")?;
    let directory = PathBuf::from(directory);
    fs::create_dir_all(&directory).map_err(|error| format!("Не удалось создать папку: {error}"))?;
    for (id, part) in message.attachments().enumerate() {
        let extension = part
            .content_type()
            .map(
                |value| match (value.c_type.as_ref(), value.c_subtype.as_deref()) {
                    ("image", Some("png")) => ".png",
                    ("image", Some("jpeg")) => ".jpg",
                    ("image", Some("gif")) => ".gif",
                    ("application", Some("pdf")) => ".pdf",
                    ("text", Some("plain")) => ".txt",
                    _ => "",
                },
            )
            .unwrap_or_default();
        let filename = safe_filename(
            part.attachment_name()
                .unwrap_or(&format!("Вложение {}{extension}", id + 1)),
        );
        fs::write(directory.join(filename), part.contents())
            .map_err(|error| format!("Не удалось сохранить вложение: {error}"))?;
    }
    Ok(message.attachment_count())
}

#[tauri::command]
pub fn inspect_attachments(paths: Vec<String>) -> Result<Vec<LocalAttachment>, String> {
    paths
        .into_iter()
        .map(|raw_path| {
            let path = PathBuf::from(&raw_path);
            let size = fs::metadata(&path)
                .map_err(|_| format!("Не удалось открыть вложение: {raw_path}"))?
                .len();
            let filename = path
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or("Некорректное имя вложения")?
                .to_string();
            Ok(LocalAttachment {
                path: raw_path,
                filename,
                size,
            })
        })
        .collect()
}

#[tauri::command]
pub async fn prepare_forward_attachments(
    raw_path: String,
    uid: u32,
) -> Result<Vec<LocalAttachment>, String> {
    let raw = fetch_raw_message(&raw_path, uid).await?;
    let message = MessageParser::default()
        .parse(&raw)
        .ok_or("Не удалось разобрать письмо")?;
    let root = dirs::cache_dir()
        .ok_or("Не удалось определить папку кеша")?
        .join("yandex-mail")
        .join("forward")
        .join(uid.to_string());
    message
        .attachments()
        .enumerate()
        .map(|(id, part)| {
            let extension = part
                .content_type()
                .map(
                    |value| match (value.c_type.as_ref(), value.c_subtype.as_deref()) {
                        ("image", Some("png")) => ".png",
                        ("image", Some("jpeg")) => ".jpg",
                        ("image", Some("gif")) => ".gif",
                        ("application", Some("pdf")) => ".pdf",
                        ("text", Some("plain")) => ".txt",
                        _ => "",
                    },
                )
                .unwrap_or_default();
            let filename = safe_filename(
                part.attachment_name()
                    .unwrap_or(&format!("Вложение {}{extension}", id + 1)),
            );
            let destination = root.join(id.to_string()).join(&filename);
            fs::create_dir_all(destination.parent().ok_or("Некорректный путь вложения")?)
                .map_err(|error| format!("Не удалось создать папку: {error}"))?;
            fs::write(&destination, part.contents())
                .map_err(|error| format!("Не удалось подготовить вложение: {error}"))?;
            Ok(LocalAttachment {
                path: destination.to_string_lossy().to_string(),
                filename,
                size: part.len() as u64,
            })
        })
        .collect()
}

#[tauri::command]
pub async fn list_labels(_raw_path: String) -> Result<Vec<Label>, String> {
    let account = read_account()?;
    let mut counts = BTreeMap::<String, u32>::new();
    for label in cache::read_labels(&account.email) {
        counts.insert(label.name, label.count);
    }
    Ok(counts
        .into_iter()
        .map(|(name, count)| Label { name, count })
        .collect())
}

#[tauri::command]
pub fn get_cached_labels() -> Result<Vec<Label>, String> {
    let account = read_account()?;
    Ok(cache::read_labels(&account.email))
}

#[tauri::command]
pub fn create_label(name: String) -> Result<Vec<Label>, String> {
    let account = read_account()?;
    let name = name.trim();
    encode_label(name)?;
    cache::create_label(&account.email, name)
}

#[tauri::command]
pub async fn rename_label(
    _raw_path: String,
    old_name: String,
    new_name: String,
) -> Result<Vec<Label>, String> {
    let old_name = old_name.trim();
    let new_name = new_name.trim();
    let old_encoded = encode_label(old_name)?;
    let new_encoded = encode_label(new_name)?;
    if old_name == new_name {
        return get_cached_labels();
    }
    let (account, mut session) = connect().await?;
    let mailboxes = session
        .list(Some(""), Some("*"))
        .await
        .map_err(|error| error.to_string())?
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .filter_map(Result::ok)
        .filter(|name| {
            !name
                .attributes()
                .iter()
                .any(|attribute| matches!(attribute, async_imap::types::NameAttribute::NoSelect))
        })
        .map(|name| name.name().to_string())
        .collect::<Vec<_>>();
    for mailbox in &mailboxes {
        session
            .select(mailbox)
            .await
            .map_err(|error| error.to_string())?;
        let mut uids = session
            .uid_search(format!("KEYWORD {old_encoded}"))
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect::<Vec<_>>();
        if !uids.is_empty() {
            let set = uid_set(&mut uids);
            let add = session
                .uid_store(&set, format!("+FLAGS.SILENT ({new_encoded})"))
                .await
                .map_err(|error| error.to_string())?;
            let _: Vec<_> = add.collect().await;
            let remove = session
                .uid_store(&set, format!("-FLAGS.SILENT ({old_encoded})"))
                .await
                .map_err(|error| error.to_string())?;
            let _: Vec<_> = remove.collect().await;
        }
    }
    let _ = session.logout().await;
    for mailbox in mailboxes {
        cache::rename_message_label(&account.email, &mailbox, old_name, new_name);
    }
    cache::rename_label(&account.email, old_name, new_name)
}

#[tauri::command]
pub async fn delete_label(raw_path: String, name: String) -> Result<Vec<Label>, String> {
    let name = name.trim();
    let encoded = encode_label(name)?;
    let (account, mut session) = connect().await?;
    session
        .select(&raw_path)
        .await
        .map_err(|error| error.to_string())?;
    let mut uids = session
        .uid_search(format!("KEYWORD {encoded}"))
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .collect::<Vec<_>>();
    if !uids.is_empty() {
        let set = uid_set(&mut uids);
        let remove = session
            .uid_store(&set, format!("-FLAGS.SILENT ({encoded})"))
            .await
            .map_err(|error| error.to_string())?;
        let _: Vec<_> = remove.collect().await;
    }
    let _ = session.logout().await;
    cache::delete_message_label(&account.email, &raw_path, name);
    cache::delete_label(&account.email, name)
}

#[tauri::command]
pub async fn set_flag(
    raw_path: String,
    uid: u32,
    flag: String,
    enabled: bool,
) -> Result<(), String> {
    let (_, mut session) = connect().await?;
    session
        .select(&raw_path)
        .await
        .map_err(|error| error.to_string())?;
    let value = match flag.as_str() {
        "seen" => "\\Seen",
        "flagged" => "\\Flagged",
        _ => return Err("Неизвестный флаг".to_string()),
    };
    let query = format!("{}FLAGS.SILENT ({value})", if enabled { "+" } else { "-" });
    let stream = session
        .uid_store(uid.to_string(), query)
        .await
        .map_err(|error| error.to_string())?;
    let _: Vec<_> = stream.collect().await;
    let _ = session.logout().await;
    Ok(())
}

#[tauri::command]
pub async fn set_label(
    raw_path: String,
    uid: u32,
    label: String,
    enabled: bool,
) -> Result<(), String> {
    let encoded = encode_label(&label)?;
    let (_, mut session) = connect().await?;
    session
        .select(&raw_path)
        .await
        .map_err(|error| error.to_string())?;
    let query = format!(
        "{}FLAGS.SILENT ({encoded})",
        if enabled { "+" } else { "-" }
    );
    let stream = session
        .uid_store(uid.to_string(), query)
        .await
        .map_err(|error| error.to_string())?;
    let _: Vec<_> = stream.collect().await;
    let _ = session.logout().await;
    Ok(())
}

#[tauri::command]
pub async fn move_message(raw_path: String, uid: u32, destination: String) -> Result<(), String> {
    let (_, mut session) = connect().await?;
    session
        .select(&raw_path)
        .await
        .map_err(|error| error.to_string())?;
    session
        .uid_mv(uid.to_string(), destination)
        .await
        .map_err(|error| error.to_string())?;
    let _ = session.logout().await;
    Ok(())
}

fn uid_set(uids: &mut Vec<u32>) -> String {
    uids.sort_unstable();
    uids.dedup();
    let mut ranges = Vec::new();
    let mut start = None;
    let mut previous = 0;
    for uid in uids.iter().copied() {
        match start {
            None => {
                start = Some(uid);
                previous = uid;
            }
            Some(_) if uid == previous.saturating_add(1) => previous = uid,
            Some(first) => {
                ranges.push(if first == previous {
                    first.to_string()
                } else {
                    format!("{first}:{previous}")
                });
                start = Some(uid);
                previous = uid;
            }
        }
    }
    if let Some(first) = start {
        ranges.push(if first == previous {
            first.to_string()
        } else {
            format!("{first}:{previous}")
        });
    }
    ranges.join(",")
}

async fn special_folder(session: &mut ImapSession, wanted: &str) -> Result<String, String> {
    use async_imap::types::NameAttribute;
    let stream = session
        .list(Some(""), Some("*"))
        .await
        .map_err(|error| error.to_string())?;
    let folders = stream.collect::<Vec<_>>().await;
    folders
        .into_iter()
        .filter_map(Result::ok)
        .find(|folder| {
            folder.attributes().iter().any(|attribute| {
                matches!(
                    (wanted, attribute),
                    ("trash", NameAttribute::Trash)
                        | ("archive", NameAttribute::Archive)
                        | ("junk", NameAttribute::Junk)
                )
            })
        })
        .map(|folder| folder.name().to_string())
        .ok_or_else(|| "Нужная системная папка не найдена".to_string())
}

async fn execute_message_action(
    session: &mut ImapSession,
    raw_path: &str,
    selection: MessageSelection,
    action: MessageAction,
) -> Result<(Vec<u32>, &'static str, Option<bool>, Option<String>), String> {
    session
        .select(raw_path)
        .await
        .map_err(|error| error.to_string())?;
    let mut uids = match selection {
        MessageSelection::Uids { uids } => uids,
        MessageSelection::All { label, query } => session
            .uid_search(message_search(label.as_deref(), query.as_deref())?)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect(),
    };
    if uids.is_empty() {
        return Ok((uids, "", None, None));
    }
    let set = uid_set(&mut uids);
    let (cache_action, cache_enabled, cache_value) = match action {
        MessageAction::Seen { enabled } => {
            let query = format!("{}FLAGS.SILENT (\\Seen)", if enabled { "+" } else { "-" });
            let stream = session
                .uid_store(&set, query)
                .await
                .map_err(|error| error.to_string())?;
            let _: Vec<_> = stream.collect().await;
            ("seen", Some(enabled), None)
        }
        MessageAction::Flagged { enabled } => {
            let query = format!(
                "{}FLAGS.SILENT (\\Flagged)",
                if enabled { "+" } else { "-" }
            );
            let stream = session
                .uid_store(&set, query)
                .await
                .map_err(|error| error.to_string())?;
            let _: Vec<_> = stream.collect().await;
            ("flagged", Some(enabled), None)
        }
        MessageAction::Label { name, enabled } => {
            let encoded = encode_label(&name)?;
            let query = format!(
                "{}FLAGS.SILENT ({encoded})",
                if enabled { "+" } else { "-" }
            );
            let stream = session
                .uid_store(&set, query)
                .await
                .map_err(|error| error.to_string())?;
            let _: Vec<_> = stream.collect().await;
            ("label", Some(enabled), Some(name))
        }
        MessageAction::Move { destination } => {
            session
                .uid_mv(&set, destination)
                .await
                .map_err(|error| error.to_string())?;
            ("move", None, None)
        }
        MessageAction::Delete => {
            let destination = special_folder(session, "trash").await?;
            if destination == raw_path {
                return Err("Письмо уже находится в корзине".to_string());
            }
            session
                .uid_mv(&set, destination)
                .await
                .map_err(|error| error.to_string())?;
            ("move", None, None)
        }
        MessageAction::Archive => {
            let destination = special_folder(session, "archive").await?;
            session
                .uid_mv(&set, destination)
                .await
                .map_err(|error| error.to_string())?;
            ("move", None, None)
        }
        MessageAction::Spam => {
            let destination = special_folder(session, "junk").await?;
            session
                .uid_mv(&set, destination)
                .await
                .map_err(|error| error.to_string())?;
            ("move", None, None)
        }
    };
    Ok((uids, cache_action, cache_enabled, cache_value))
}

#[tauri::command]
pub async fn apply_message_action(
    raw_path: String,
    selection: MessageSelection,
    action: MessageAction,
) -> Result<ActionResult, String> {
    let (account, mut session) = connect().await?;
    let result = tokio::time::timeout(
        TIMEOUT,
        execute_message_action(&mut session, &raw_path, selection, action),
    )
    .await;
    let (uids, cache_action, cache_enabled, cache_value) = match result {
        Ok(result) => result?,
        Err(_) => {
            let _ = tokio::time::timeout(Duration::from_secs(2), session.logout()).await;
            return Err(
                "ACTION_OUTCOME_UNKNOWN: Сервер не подтвердил результат действия вовремя"
                    .to_string(),
            );
        }
    };
    let affected = uids.len();
    let uid_lookup = uids.iter().copied().collect::<HashSet<_>>();
    let _ = session.logout().await;
    if affected > 0 {
        cache::update_messages(
            &account.email,
            &raw_path,
            &uid_lookup,
            cache_action,
            cache_enabled,
            cache_value.as_deref(),
        );
    }
    Ok(ActionResult { affected })
}

#[tauri::command]
pub fn watch_folder(
    app: AppHandle,
    state: State<'_, WatchState>,
    raw_path: String,
) -> Result<(), String> {
    let mut current = state
        .0
        .lock()
        .map_err(|_| "Не удалось запустить ожидание писем".to_string())?;
    if let Some(handle) = current.take() {
        handle.abort();
    }
    let handle = tauri::async_runtime::spawn(async move {
        loop {
            let Ok((_, mut session)) = connect().await else {
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            };
            let supports_idle = session
                .capabilities()
                .await
                .map(|capabilities| capabilities.has_str("IDLE"))
                .unwrap_or(false);
            if !supports_idle || session.select(&raw_path).await.is_err() {
                let _ = session.logout().await;
                return;
            }
            let mut idle = session.idle();
            if idle.init().await.is_err() {
                return;
            }
            let (wait, _interrupt) = idle.wait();
            let changed = matches!(
                wait.await,
                Ok(async_imap::extensions::idle::IdleResponse::NewData(_))
            );
            let _ = idle.done().await;
            if changed {
                let _ = app.emit("mail-changed", &raw_path);
            }
        }
    });
    *current = Some(handle);
    Ok(())
}

#[tauri::command]
pub fn stop_watching(state: State<'_, WatchState>) -> Result<(), String> {
    if let Some(handle) = state
        .0
        .lock()
        .map_err(|_| "Не удалось остановить ожидание писем".to_string())?
        .take()
    {
        handle.abort();
    }
    Ok(())
}

fn build_outgoing(from_email: &str, outgoing: OutgoingMessage) -> Result<Message, String> {
    if outgoing.to.trim().is_empty() || outgoing.subject.contains(['\r', '\n']) {
        return Err("Укажите получателя и корректную тему".to_string());
    }
    let from: Mailbox = from_email
        .parse()
        .map_err(|_| "Некорректный адрес отправителя".to_string())?;
    let recipients = outgoing
        .to
        .split([',', ';'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<Mailbox>()
                .map_err(|_| format!("Некорректный адрес: {value}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if recipients.is_empty() {
        return Err("Укажите получателя".to_string());
    }

    let mut builder = Message::builder().from(from).subject(outgoing.subject);
    for recipient in recipients {
        builder = builder.to(recipient);
    }
    let plain = SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .body(outgoing.text);
    let mut body = MultiPart::mixed().multipart(
        match outgoing.html.filter(|value| !value.trim().is_empty()) {
            Some(html) => MultiPart::alternative().singlepart(plain).singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_HTML)
                    .body(html),
            ),
            None => MultiPart::alternative().singlepart(plain),
        },
    );
    let mut total_size = 0_u64;
    for raw_path in outgoing.attachments {
        let path = PathBuf::from(&raw_path);
        let metadata =
            fs::metadata(&path).map_err(|_| format!("Не удалось открыть вложение: {raw_path}"))?;
        total_size = total_size.saturating_add(metadata.len());
        if total_size > 25 * 1024 * 1024 {
            return Err("Общий размер вложений больше 25 МБ".to_string());
        }
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or("Некорректное имя вложения")?;
        let content_type =
            ContentType::parse("application/octet-stream").map_err(|error| error.to_string())?;
        body = body.singlepart(LettreAttachment::new(filename.to_string()).body(
            fs::read(&path).map_err(|_| format!("Не удалось прочитать вложение: {raw_path}"))?,
            content_type,
        ));
    }
    builder
        .multipart(body)
        .map_err(|error| format!("Не удалось подготовить письмо: {error}"))
}

#[tauri::command]
pub async fn send_message(outgoing: OutgoingMessage) -> Result<(), String> {
    let (account, password) = load_account()?;
    let message = build_outgoing(&account.email, outgoing)?;
    let transport = AsyncSmtpTransport::<Tokio1Executor>::relay(&account.smtp_host)
        .map_err(|error| format!("Не удалось подготовить отправку: {error}"))?
        .port(account.smtp_port)
        .credentials(Credentials::new(account.email, password))
        .build();
    tokio::time::timeout(TIMEOUT, transport.send(message))
        .await
        .map_err(|_| "Сервер отправки не ответил вовремя".to_string())?
        .map(|_| ())
        .map_err(|error| format!("Не удалось отправить письмо: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_imap::imap_proto::types::{
        BodyContentCommon, BodyContentSinglePart, ContentDisposition, ContentEncoding, ContentType,
    };

    #[test]
    fn label_codec_matches_email_cli() {
        let encoded = encode_label("с_хуями").unwrap();
        assert_eq!(encoded, "emailcli-0YFf0YXRg9GP0LzQuA");
        assert_eq!(
            user_label(Flag::Custom(encoded.into())).as_deref(),
            Some("с_хуями")
        );
        assert_eq!(user_label(Flag::Custom("system_hamon".into())), None);
    }

    #[test]
    fn outgoing_message_preserves_cyrillic() {
        let root =
            std::env::temp_dir().join(format!("yandex-mail-outgoing-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let first = root.join("файл-1.txt");
        let second = root.join("файл-2.txt");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"two").unwrap();
        let message = build_outgoing(
            "sender@example.com",
            OutgoingMessage {
                to: "receiver@example.com".to_string(),
                subject: "Тема проверки".to_string(),
                text: "Привет, мир!".to_string(),
                html: Some("<b>Привет, мир!</b>".to_string()),
                attachments: vec![
                    first.to_string_lossy().to_string(),
                    second.to_string_lossy().to_string(),
                ],
            },
        )
        .unwrap();
        let formatted = message.formatted();
        let parsed = MessageParser::default().parse(&formatted).unwrap();
        assert_eq!(parsed.subject(), Some("Тема проверки"));
        assert!(parsed.body_text(0).unwrap().contains("Привет, мир!"));
        assert_eq!(parsed.attachment_count(), 2);
        assert_eq!(
            parsed
                .attachments()
                .filter_map(|part| part.attachment_name())
                .collect::<Vec<_>>(),
            ["файл-1.txt", "файл-2.txt"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uid_ranges_are_compact() {
        assert_eq!(uid_set(&mut vec![9, 2, 3, 4, 9, 11]), "2:4,9,11");
    }

    #[test]
    fn search_uses_indexable_mail_fields_without_downloading_messages() {
        assert_eq!(
            message_search(None, Some("чек")).unwrap(),
            "CHARSET UTF-8 OR FROM \"чек\" OR TO \"чек\" OR CC \"чек\" OR BCC \"чек\" OR SUBJECT \"чек\" BODY \"чек\""
        );
        assert_eq!(
            message_search(Some("важное"), Some("a\\\"b")).unwrap(),
            format!(
                "KEYWORD {} OR FROM \"a\\\\\\\"b\" OR TO \"a\\\\\\\"b\" OR CC \"a\\\\\\\"b\" OR BCC \"a\\\\\\\"b\" OR SUBJECT \"a\\\\\\\"b\" BODY \"a\\\\\\\"b\"",
                encode_label("важное").unwrap()
            )
        );
    }

    #[test]
    fn yandex_label_names_follow_the_visible_limit() {
        assert!(encode_label("123456789012345").is_ok());
        assert!(encode_label("1234567890123456").is_err());
    }

    #[test]
    fn folder_names_are_validated_and_encoded() {
        assert_eq!(folder_name(" Тест ".to_string()).unwrap(), "&BCIENQRBBEI-");
        assert!(folder_name("bad\nname".to_string()).is_err());
        assert!(folder_name(" ".to_string()).is_err());
    }

    #[test]
    fn selection_contract_keeps_explicit_and_full_scopes_distinct() {
        let explicit: MessageSelection =
            serde_json::from_str(r#"{"kind":"uids","uids":[7,9]}"#).unwrap();
        assert!(matches!(explicit, MessageSelection::Uids { uids } if uids == [7, 9]));
        let full: MessageSelection =
            serde_json::from_str(r#"{"kind":"all","label":"с_хуями","query":"чек"}"#).unwrap();
        assert!(
            matches!(full, MessageSelection::All { label: Some(label), query: Some(query) } if label == "с_хуями" && query == "чек")
        );
    }

    #[test]
    fn account_file_replacement_cleans_temporary_state() {
        let directory =
            std::env::temp_dir().join(format!("yandex-mail-account-config-{}", std::process::id()));
        let path = directory.join("account.json");
        replace_file(&path, b"old").unwrap();
        replace_file(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(!path.with_extension("new").exists());
        assert!(!path.with_extension("old").exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn attachment_is_extracted() {
        let raw = b"From: sender@example.com\r\nTo: receiver@example.com\r\nSubject: attachment\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nbody\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename*=UTF-8''%D1%84%D0%B0%D0%B9%D0%BB.txt\r\n\r\nhello\r\n--x--\r\n";
        let parsed = MessageParser::default().parse(raw).unwrap();
        assert_eq!(
            parsed.attachments().next().unwrap().attachment_name(),
            Some("файл.txt")
        );
        let path =
            std::env::temp_dir().join(format!("yandex-mail-attachment-{}.txt", std::process::id()));
        write_attachment(raw, 0, &path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn inline_named_binary_part_is_an_attachment() {
        let body = BodyStructure::Basic {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: "image".into(),
                    subtype: "png".into(),
                    params: None,
                },
                disposition: Some(ContentDisposition {
                    ty: "inline".into(),
                    params: Some(vec![("filename".into(), "фото.png".into())]),
                }),
                language: None,
                location: None,
            },
            other: BodyContentSinglePart {
                id: None,
                md5: None,
                description: None,
                transfer_encoding: ContentEncoding::Base64,
                octets: 12,
            },
            extension: None,
        };
        assert!(bodystructure_has_attachments(&body));
    }

    #[test]
    fn image_preview_is_embeddable() {
        assert_eq!(
            preview_url("image/png", b"png"),
            "data:image/png;base64,cG5n"
        );
    }

    #[test]
    #[ignore = "uses the configured real mailbox"]
    fn live_mailbox_smoke() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            test_account().await.unwrap();
            let folders = list_folders().await.unwrap();
            let inbox = folders
                .iter()
                .find(|folder| folder.special_use.as_deref() == Some("inbox"))
                .unwrap();
            let page = list_messages(inbox.raw_path.clone(), 3, None, None)
                .await
                .unwrap();
            assert_eq!(page.items.len(), 3);
            let detail = get_message(inbox.raw_path.clone(), page.items[0].uid)
                .await
                .unwrap();
            let envelope = format!(
                "{} {} {}",
                detail.summary.from_name, detail.summary.from_address, detail.summary.subject
            );
            let body_word = detail
                .body_text
                .as_deref()
                .unwrap_or_default()
                .split(|character: char| !character.is_alphanumeric())
                .find(|word| word.chars().count() > 7 && !envelope.contains(word))
                .unwrap()
                .to_string();
            let target_uid = detail.summary.uid;
            for query in [
                detail.summary.from_name,
                detail.summary.from_address,
                detail.summary.subject,
                body_word,
            ] {
                let matches = list_messages(inbox.raw_path.clone(), 3, None, Some(query.clone()))
                    .await
                    .unwrap();
                assert!(
                    matches
                        .items
                        .iter()
                        .any(|message| message.uid == target_uid),
                    "search missed the source message for {query}"
                );
            }
            let labels = list_labels(inbox.raw_path.clone()).await.unwrap();
            assert!(labels.iter().any(|label| label.name == "с_хуями"));
        });
    }
}
