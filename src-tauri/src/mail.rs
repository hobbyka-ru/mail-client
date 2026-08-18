use async_imap::{types::Flag, Client, Session};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::StreamExt;
use keyring::Entry;
use lettre::{
    message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use mail_parser::MessageParser;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;

const KEYRING_SERVICE: &str = "email-cli";
const LABEL_PREFIX: &str = "emailcli-";
const TIMEOUT: Duration = Duration::from_secs(30);

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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    name: String,
    path: String,
    raw_path: String,
    special_use: Option<String>,
    total: u32,
    unseen: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    uid: u32,
    subject: String,
    from_name: String,
    from_address: String,
    date: i64,
    seen: bool,
    flagged: bool,
    labels: Vec<String>,
    size: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    items: Vec<MessageSummary>,
    total: u32,
    unseen: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDetail {
    #[serde(flatten)]
    summary: MessageSummary,
    to: String,
    body_text: Option<String>,
    body_html: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Label {
    name: String,
    count: u32,
}

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

fn load_account() -> Result<(Account, String), String> {
    let account: Account = serde_json::from_str(
        &fs::read_to_string(config_path()?).map_err(|_| "Почта ещё не настроена".to_string())?,
    )
    .map_err(|_| "Настройки почты повреждены".to_string())?;
    let password = Entry::new(KEYRING_SERVICE, &account.email)
        .map_err(|_| "Не удалось открыть хранилище паролей".to_string())?
        .get_password()
        .map_err(|_| "Пароль приложения не найден".to_string())?;
    Ok((account, password))
}

async fn connect() -> Result<(Account, ImapSession), String> {
    let (account, password) = load_account()?;
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
    if label.is_empty() || label.chars().count() > 64 || label.contains(['\r', '\n']) {
        return Err("Некорректное название метки".to_string());
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
            "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER])",
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
    load_account().map(|(account, _)| account)
}

#[tauri::command]
pub fn save_account(account: Account, password: String) -> Result<(), String> {
    if account.email.trim().is_empty() || password.is_empty() {
        return Err("Укажите адрес и пароль приложения".to_string());
    }
    Entry::new(KEYRING_SERVICE, &account.email)
        .map_err(|_| "Не удалось открыть хранилище паролей".to_string())?
        .set_password(&password)
        .map_err(|_| "Не удалось сохранить пароль".to_string())?;
    let path = config_path()?;
    fs::create_dir_all(path.parent().ok_or("Некорректный путь настроек")?)
        .map_err(|_| "Не удалось создать папку настроек".to_string())?;
    fs::write(
        path,
        serde_json::to_vec_pretty(&account).map_err(|error| error.to_string())?,
    )
    .map_err(|_| "Не удалось сохранить настройки".to_string())
}

#[tauri::command]
pub async fn test_account() -> Result<(), String> {
    let (_, mut session) = connect().await?;
    session.logout().await.map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn list_folders() -> Result<Vec<Folder>, String> {
    let (_, mut session) = connect().await?;
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
    Ok(folders)
}

#[tauri::command]
pub async fn list_messages(
    raw_path: String,
    limit: usize,
    label: Option<String>,
    query: Option<String>,
) -> Result<MessagePage, String> {
    let (_, mut session) = connect().await?;
    let mailbox = session
        .select(&raw_path)
        .await
        .map_err(|error| format!("Не удалось открыть папку: {error}"))?;
    let unseen = session
        .uid_search("UNSEEN")
        .await
        .map_err(|error| error.to_string())?
        .len() as u32;
    let search = if let Some(label) = label.filter(|value| !value.is_empty()) {
        format!("KEYWORD {}", encode_label(&label)?)
    } else if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
        if query.contains(['\r', '\n']) {
            return Err("Некорректный поисковый запрос".to_string());
        }
        format!(
            "TEXT \"{}\"",
            query.trim().replace('\\', "\\\\").replace('"', "\\\"")
        )
    } else {
        "ALL".to_string()
    };
    let filtered = search != "ALL";
    let mut uids = session
        .uid_search(search)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .collect::<Vec<_>>();
    let total = if filtered {
        uids.len() as u32
    } else {
        mailbox.exists
    };
    uids.sort_unstable();
    let start = uids.len().saturating_sub(limit.clamp(1, 100));
    let items = fetch_summaries(&mut session, &raw_path, &uids[start..]).await?;
    let _ = session.logout().await;
    Ok(MessagePage {
        items,
        total,
        unseen,
    })
}

#[tauri::command]
pub async fn get_message(raw_path: String, uid: u32) -> Result<MessageDetail, String> {
    let (_, mut session) = connect().await?;
    session
        .select(&raw_path)
        .await
        .map_err(|error| error.to_string())?;
    let stream = session
        .uid_fetch(
            uid.to_string(),
            "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[])",
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
    };
    let _ = session.logout().await;
    Ok(detail)
}

#[tauri::command]
pub async fn list_labels(raw_path: String) -> Result<Vec<Label>, String> {
    let (_, mut session) = connect().await?;
    session
        .select(&raw_path)
        .await
        .map_err(|error| error.to_string())?;
    let stream = session
        .uid_fetch("1:*", "(UID FLAGS)")
        .await
        .map_err(|error| error.to_string())?;
    let fetched = stream.collect::<Vec<_>>().await;
    let mut counts = BTreeMap::<String, u32>::new();
    for fetch in fetched.into_iter().filter_map(Result::ok) {
        for label in fetch.flags().filter_map(user_label) {
            *counts.entry(label).or_default() += 1;
        }
    }
    let _ = session.logout().await;
    Ok(counts
        .into_iter()
        .map(|(name, count)| Label { name, count })
        .collect())
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
        body = body.singlepart(Attachment::new(filename.to_string()).body(
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
    transport
        .send(message)
        .await
        .map(|_| ())
        .map_err(|error| format!("Не удалось отправить письмо: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let message = build_outgoing(
            "sender@example.com",
            OutgoingMessage {
                to: "receiver@example.com".to_string(),
                subject: "Тема проверки".to_string(),
                text: "Привет, мир!".to_string(),
                html: Some("<b>Привет, мир!</b>".to_string()),
                attachments: Vec::new(),
            },
        )
        .unwrap();
        let formatted = message.formatted();
        let parsed = MessageParser::default().parse(&formatted).unwrap();
        assert_eq!(parsed.subject(), Some("Тема проверки"));
        assert!(parsed.body_text(0).unwrap().contains("Привет, мир!"));
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
            let labels = list_labels(inbox.raw_path.clone()).await.unwrap();
            assert!(labels.iter().any(|label| label.name == "с_хуями"));
        });
    }
}
