use crate::mail::{Folder, Label, MessageDetail, MessagePage, MessageSummary};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

pub(crate) const CACHE_VERSION: u8 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedPage {
    pub version: u8,
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub page: MessagePage,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedDetail {
    version: u8,
    uid_validity: Option<u32>,
    detail: MessageDetail,
}

fn root(email: &str) -> Result<PathBuf, String> {
    dirs::cache_dir()
        .map(|path| path.join("yandex-mail").join(URL_SAFE_NO_PAD.encode(email)))
        .ok_or_else(|| "Не удалось определить папку кеша".to_string())
}

fn scope(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value)
}

fn read_json<T: DeserializeOwned>(path: PathBuf) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_json<T: Serialize>(path: PathBuf, value: &T) -> Result<(), String> {
    let parent = path.parent().ok_or("Некорректный путь кеша")?;
    fs::create_dir_all(parent).map_err(|error| format!("Не удалось создать кеш: {error}"))?;
    let temporary = path.with_extension("new");
    fs::write(
        &temporary,
        serde_json::to_vec(value)
            .map_err(|error| format!("Не удалось подготовить кеш: {error}"))?,
    )
    .map_err(|error| format!("Не удалось записать кеш: {error}"))?;
    if path.exists() {
        // Cache is disposable: this Windows-compatible replacement avoids ever parsing a partial file.
        let _ = fs::remove_file(&path);
    }
    fs::rename(&temporary, &path).map_err(|error| format!("Не удалось сохранить кеш: {error}"))
}

pub fn read_folders(email: &str) -> Vec<Folder> {
    root(email)
        .ok()
        .and_then(|path| read_json(path.join("folders.json")))
        .unwrap_or_default()
}

pub fn write_folders(email: &str, folders: &[Folder]) -> Result<(), String> {
    write_json(root(email)?.join("folders.json"), &folders)
}

pub fn read_labels(email: &str) -> Vec<Label> {
    root(email)
        .ok()
        .and_then(|path| read_json(path.join("labels.json")))
        .unwrap_or_default()
}

pub fn write_labels(email: &str, labels: &[Label]) -> Result<(), String> {
    write_json(root(email)?.join("labels.json"), &labels)
}

pub fn create_label(email: &str, name: &str) -> Result<Vec<Label>, String> {
    let mut labels = read_labels(email);
    if labels.iter().any(|label| label.name == name) {
        return Err("Метка с таким названием уже есть".to_string());
    }
    labels.push(Label {
        name: name.to_string(),
        count: 0,
    });
    labels.sort_by(|left, right| left.name.cmp(&right.name));
    write_labels(email, &labels)?;
    Ok(labels)
}

pub fn rename_label(email: &str, old_name: &str, new_name: &str) -> Result<Vec<Label>, String> {
    let mut labels = read_labels(email);
    let old_count = labels
        .iter()
        .find(|label| label.name == old_name)
        .map(|label| label.count)
        .unwrap_or_default();
    labels.retain(|label| label.name != old_name);
    if let Some(label) = labels.iter_mut().find(|label| label.name == new_name) {
        label.count = label.count.saturating_add(old_count);
    } else {
        labels.push(Label {
            name: new_name.to_string(),
            count: old_count,
        });
    }
    labels.sort_by(|left, right| left.name.cmp(&right.name));
    write_labels(email, &labels)?;
    Ok(labels)
}

pub fn delete_label(email: &str, name: &str) -> Result<Vec<Label>, String> {
    let mut labels = read_labels(email);
    labels.retain(|label| label.name != name);
    write_labels(email, &labels)?;
    Ok(labels)
}

pub fn read_page(email: &str, raw_path: &str) -> Option<CachedPage> {
    let cached: CachedPage = read_json(
        root(email)
            .ok()?
            .join("folders")
            .join(format!("{}.json", scope(raw_path))),
    )?;
    (cached.version == CACHE_VERSION).then_some(cached)
}

pub fn write_page(email: &str, raw_path: &str, page: &CachedPage) -> Result<(), String> {
    write_json(
        root(email)?
            .join("folders")
            .join(format!("{}.json", scope(raw_path))),
        page,
    )
}

pub fn search_messages(email: &str, raw_path: &str, query: &str) -> Vec<MessageSummary> {
    let needle = query.trim().to_lowercase();
    let Some(cached) = read_page(email, raw_path) else {
        return Vec::new();
    };
    let uid_validity = cached.uid_validity;
    let details = root(email)
        .ok()
        .map(|path| path.join("details").join(scope(raw_path)));
    cached
        .page
        .items
        .into_iter()
        .filter(|message| {
            let summary_matches = [
                message.from_name.as_str(),
                message.from_address.as_str(),
                message.subject.as_str(),
            ]
            .iter()
            .any(|value| value.to_lowercase().contains(&needle));
            summary_matches
                || details
                    .as_deref()
                    .and_then(|path| read_detail_at(path, message.uid, uid_validity))
                    .is_some_and(|detail| {
                        detail
                            .body_text
                            .as_deref()
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&needle)
                            || detail
                                .body_html
                                .as_deref()
                                .unwrap_or_default()
                                .to_lowercase()
                                .contains(&needle)
                    })
        })
        .collect()
}

pub fn read_detail(email: &str, raw_path: &str, uid: u32) -> Option<MessageDetail> {
    let page = read_page(email, raw_path)?;
    read_detail_at(
        &root(email).ok()?.join("details").join(scope(raw_path)),
        uid,
        page.uid_validity,
    )
}

fn read_detail_at(path: &Path, uid: u32, uid_validity: Option<u32>) -> Option<MessageDetail> {
    let cached: CachedDetail = read_json(path.join(format!("{uid}.json")))?;
    (cached.version == CACHE_VERSION && cached.uid_validity == uid_validity)
        .then_some(cached.detail)
}

pub fn write_detail(
    email: &str,
    raw_path: &str,
    uid_validity: Option<u32>,
    detail: &MessageDetail,
) -> Result<(), String> {
    write_json(
        root(email)?
            .join("details")
            .join(scope(raw_path))
            .join(format!("{}.json", detail.summary.uid)),
        &CachedDetail {
            version: CACHE_VERSION,
            uid_validity,
            detail: detail.clone(),
        },
    )
}

pub fn update_messages(
    email: &str,
    raw_path: &str,
    uids: &HashSet<u32>,
    action: &str,
    enabled: Option<bool>,
    value: Option<&str>,
) {
    let Some(mut cached) = read_page(email, raw_path) else {
        return;
    };
    match action {
        "move" => {
            let before = cached.page.items.len();
            cached
                .page
                .items
                .retain(|message| !uids.contains(&message.uid));
            let removed = before.saturating_sub(cached.page.items.len()) as u32;
            cached.page.total = cached.page.total.saturating_sub(removed);
        }
        "seen" => patch(&mut cached.page.items, uids, |message| {
            message.seen = enabled.unwrap_or(true)
        }),
        "flagged" => patch(&mut cached.page.items, uids, |message| {
            message.flagged = enabled.unwrap_or(true)
        }),
        "label" => patch(&mut cached.page.items, uids, |message| {
            if let Some(label) = value {
                if enabled.unwrap_or(true) && !message.labels.iter().any(|item| item == label) {
                    message.labels.push(label.to_string());
                } else if !enabled.unwrap_or(true) {
                    message.labels.retain(|item| item != label);
                }
            }
        }),
        _ => {}
    }
    let _ = write_page(email, raw_path, &cached);
}

pub fn rename_message_label(email: &str, raw_path: &str, old_name: &str, new_name: &str) {
    let Some(mut cached) = read_page(email, raw_path) else {
        return;
    };
    for message in &mut cached.page.items {
        if message.labels.iter().any(|label| label == old_name) {
            message.labels.retain(|label| label != old_name);
            if !message.labels.iter().any(|label| label == new_name) {
                message.labels.push(new_name.to_string());
            }
        }
    }
    let _ = write_page(email, raw_path, &cached);
}

pub fn delete_message_label(email: &str, raw_path: &str, name: &str) {
    let Some(mut cached) = read_page(email, raw_path) else {
        return;
    };
    for message in &mut cached.page.items {
        message.labels.retain(|label| label != name);
    }
    let _ = write_page(email, raw_path, &cached);
}

fn patch(
    messages: &mut [MessageSummary],
    uids: &HashSet<u32>,
    mut apply: impl FnMut(&mut MessageSummary),
) {
    for message in messages
        .iter_mut()
        .filter(|message| uids.contains(&message.uid))
    {
        apply(message);
    }
}

pub fn purge(email: &str) {
    if let Ok(path) = root(email) {
        let _ = fs::remove_dir_all(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_leaves_one_complete_file() {
        let directory =
            std::env::temp_dir().join(format!("yandex-mail-atomic-cache-{}", std::process::id()));
        let path = directory.join("snapshot.json");
        write_json(path.clone(), &vec!["first"]).unwrap();
        write_json(path.clone(), &vec!["second"]).unwrap();
        assert_eq!(read_json::<Vec<String>>(path.clone()).unwrap(), ["second"]);
        assert!(!path.with_extension("new").exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn invalid_json_is_a_cache_miss() {
        let path =
            std::env::temp_dir().join(format!("yandex-mail-cache-{}.json", std::process::id()));
        fs::write(&path, b"not-json").unwrap();
        assert!(read_json::<CachedPage>(path.clone()).is_none());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn empty_labels_can_be_created_renamed_and_deleted() {
        let email = format!("label-cache-test-{}@example.com", std::process::id());
        assert_eq!(create_label(&email, "новая").unwrap()[0].name, "новая");
        assert_eq!(
            rename_label(&email, "новая", "готово").unwrap()[0].name,
            "готово"
        );
        assert!(delete_label(&email, "готово").unwrap().is_empty());
        purge(&email);
    }

    #[test]
    fn rename_repairs_a_partially_created_destination_label() {
        let email = format!("label-merge-test-{}@example.com", std::process::id());
        create_label(&email, "старая").unwrap();
        create_label(&email, "новая").unwrap();
        let labels = rename_label(&email, "старая", "новая").unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].name, "новая");
        purge(&email);
    }

    #[test]
    fn uid_validity_invalidates_cached_detail() {
        let email = format!("cache-test-{}@example.com", std::process::id());
        let raw_path = "INBOX";
        let summary = MessageSummary {
            uid: 7,
            subject: "Тест".into(),
            from_name: String::new(),
            from_address: "sender@example.com".into(),
            date: 0,
            seen: false,
            flagged: false,
            labels: Vec::new(),
            size: 10,
            has_attachments: false,
        };
        let detail = MessageDetail {
            summary: summary.clone(),
            to: "receiver@example.com".into(),
            body_text: Some("body".into()),
            body_html: None,
            attachments: Vec::new(),
        };
        write_page(
            &email,
            raw_path,
            &CachedPage {
                version: CACHE_VERSION,
                uid_validity: Some(1),
                uid_next: Some(8),
                page: MessagePage {
                    items: vec![summary.clone()],
                    total: 1,
                    unseen: 1,
                },
            },
        )
        .unwrap();
        write_detail(&email, raw_path, Some(1), &detail).unwrap();
        assert!(read_detail(&email, raw_path, 7).is_some());
        write_page(
            &email,
            raw_path,
            &CachedPage {
                version: CACHE_VERSION,
                uid_validity: Some(2),
                uid_next: Some(8),
                page: MessagePage {
                    items: vec![summary],
                    total: 1,
                    unseen: 1,
                },
            },
        )
        .unwrap();
        assert!(read_detail(&email, raw_path, 7).is_none());
        purge(&email);
    }

    #[test]
    #[ignore = "performance benchmark"]
    fn cached_full_text_search_benchmark() {
        use std::time::Instant;

        let email = format!("search-perf-{}@example.com", std::process::id());
        let raw_path = "INBOX";
        let items = (1..=60)
            .map(|uid| MessageSummary {
                uid,
                subject: format!("Письмо {uid}"),
                from_name: "Отправитель".into(),
                from_address: "sender@example.com".into(),
                date: uid as i64,
                seen: false,
                flagged: false,
                labels: Vec::new(),
                size: 1024,
                has_attachments: false,
            })
            .collect::<Vec<_>>();
        write_page(
            &email,
            raw_path,
            &CachedPage {
                version: CACHE_VERSION,
                uid_validity: Some(1),
                uid_next: Some(61),
                page: MessagePage {
                    items: items.clone(),
                    total: 60,
                    unseen: 60,
                },
            },
        )
        .unwrap();
        for summary in items {
            write_detail(
                &email,
                raw_path,
                Some(1),
                &MessageDetail {
                    summary,
                    to: "receiver@example.com".into(),
                    body_text: Some("Обычный текст письма".into()),
                    body_html: None,
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        }

        let started = Instant::now();
        for _ in 0..100 {
            assert!(search_messages(&email, raw_path, "несуществующая строка").is_empty());
        }
        println!("cached_full_text_search_100={:?}", started.elapsed());
        purge(&email);
    }
}
